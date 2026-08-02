//! End-of-day digest builder for the global activity tree (PG-backed).
//!
//! Ported from `memory::tree::tree_global::digest` - same logic, stores swapped
//! to the PG async stores. Once per calendar day we walk every active source
//! tree, collect the summary material that covers that day, fold it into one
//! cross-source recap, and persist it as an L0 node in the singleton global
//! tree. Constants (`GLOBAL_SCOPE` etc.) and `content_store`/`Summariser` are
//! reused verbatim from the memory crate.

use std::path::Path;
use std::sync::Arc;

use deadpool_postgres::Pool;
use memory::tree::content_store::ContentStore;
use memory::tree::summariser::{Summariser, SummaryContext, SummaryInput};
use memory::tree::summariser::inert::InertSummariser;
use memory::tree::tree_global::{GLOBAL_SCOPE, GLOBAL_TOKEN_BUDGET, WEEKLY_SEAL_THRESHOLD};
use memory::tree::types::{SummaryNode, Tree};
use types::error::CarrierResult;
use types::memory_tree::TreeKind;

use crate::bucket_seal::BucketSealEngine;
use crate::pg::chunk_store::ChunkStore;
use crate::pg::tree_store::TreeStore;

/// Outcome of a single `end_of_day_digest` call.
#[derive(Debug, Clone)]
pub enum DigestOutcome {
    /// Emitted one L0 daily node and possibly cascaded into higher-level seals.
    Emitted { daily_id: String, source_count: usize },
    /// No source tree had material for the target day.
    EmptyDay,
    /// An L0 daily node already exists for this day.
    Skipped { existing_id: String },
}

/// Run an end-of-day digest for a given owner.
pub async fn end_of_day_digest(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    summariser: &dyn Summariser,
) -> CarrierResult<DigestOutcome> {
    let tree_store = TreeStore::new(pool.clone());
    let content_store = ContentStore::new(content_root.to_path_buf());
    let chunk_store = ChunkStore::new(pool.clone());

    // Get or create the global tree (owner-shared: user_id = "").
    let global = tree_store
        .get_or_create_tree(owner_id, "", TreeKind::Global, GLOBAL_SCOPE)
        .await?;

    let now_ms = chrono::Utc::now().timestamp_millis();

    // Check for an existing daily digest (idempotency)
    if let Some(existing) = find_existing_daily(&tree_store, owner_id, &global.id).await? {
        return Ok(DigestOutcome::Skipped { existing_id: existing });
    }

    // Gather one contribution per active source tree (cross-user by design -
    // the daily digest folds every user's activity under this owner).
    let source_trees = tree_store
        .list_trees(owner_id, None, Some(TreeKind::Source), 1000)
        .await?;
    let mut inputs: Vec<SummaryInput> = Vec::with_capacity(source_trees.len());

    for tree_summary in &source_trees {
        if let Some(tree) = tree_store
            .get_tree(owner_id, None, &tree_summary.tree_id)
            .await?
        {
            if let Some(inp) =
                pick_source_contribution(&tree_store, &chunk_store, &content_store, owner_id, &tree)
                    .await?
            {
                inputs.push(inp);
            }
        }
    }

    if inputs.is_empty() {
        return Ok(DigestOutcome::EmptyDay);
    }

    // Fold cross-source material into one daily recap
    let ctx = SummaryContext {
        tree_id: &global.id,
        tree_kind: TreeKind::Global,
        target_level: 0,
        token_budget: GLOBAL_TOKEN_BUDGET,
    };
    let output = summariser.summarise(&inputs, &ctx);

    // Union entities from all inputs
    let mut entities_set = std::collections::BTreeSet::new();
    let mut topics_set = std::collections::BTreeSet::new();
    for inp in &inputs {
        for e in &inp.entities {
            entities_set.insert(e.clone());
        }
        for t in &inp.topics {
            topics_set.insert(t.clone());
        }
    }

    let score = inputs
        .iter()
        .map(|i| i.score)
        .fold(f32::NEG_INFINITY, f32::max)
        .max(0.0);

    let daily_id = format!("sum_L0_{}", uuid::Uuid::new_v4().simple());
    let daily = SummaryNode {
        id: daily_id.clone(),
        tree_id: global.id.clone(),
        // Owner-shared: the daily digest spans every user under this owner.
        user_id: String::new(),
        tree_kind: TreeKind::Global,
        level: 0,
        parent_id: None,
        child_ids: inputs.iter().map(|i| i.id.clone()).collect(),
        content: output.content.clone(),
        token_count: output.token_count,
        entities: entities_set.into_iter().collect(),
        topics: topics_set.into_iter().collect(),
        time_range_start_ms: inputs.iter().map(|i| i.time_range_start_ms).min().unwrap_or(now_ms),
        time_range_end_ms: inputs.iter().map(|i| i.time_range_end_ms).max().unwrap_or(now_ms),
        score,
        sealed_at_ms: now_ms,
        deleted: false,
        embedding: None,
    };

    // Write content (file I/O - sync, reused from memory crate)
    content_store.ensure_dirs(owner_id)?;
    content_store.write_summary(owner_id, &daily)?;

    // Persist
    tree_store.insert_summary(owner_id, &daily).await?;

    // Append into the global tree's L0 buffer
    let seal_engine = BucketSealEngine::new(
        pool.clone(),
        content_root.to_path_buf(),
        Arc::new(InertSummariser),
    );
    seal_engine
        .append_to_buffer(owner_id, &global.id, 0, &daily_id, daily.token_count as i64, now_ms)
        .await?;

    // Check if weekly seal should trigger
    let buf = seal_engine
        .get_or_create_buffer(owner_id, &global.id, 0)
        .await?;
    if buf.item_ids.len() >= WEEKLY_SEAL_THRESHOLD {
        seal_engine.cascade_seals(owner_id, &global, 0, true).await?;
    }

    Ok(DigestOutcome::Emitted {
        daily_id,
        source_count: inputs.len(),
    })
}

async fn find_existing_daily(
    tree_store: &TreeStore,
    owner_id: &str,
    global_tree_id: &str,
) -> CarrierResult<Option<String>> {
    // Check for any L0 summary in the global tree today
    let summaries = tree_store
        .list_summaries(owner_id, None, global_tree_id, Some(0), 1)
        .await?;
    Ok(summaries.first().map(|s| s.id.clone()))
}

async fn pick_source_contribution(
    tree_store: &TreeStore,
    _chunk_store: &ChunkStore,
    _content_store: &ContentStore,
    owner_id: &str,
    source_tree: &Tree,
) -> CarrierResult<Option<SummaryInput>> {
    // Pick the highest-level summary (root) from this source tree
    if source_tree.root_id.is_none() {
        // No sealed summaries yet
        return Ok(None);
    }

    let root_id = source_tree.root_id.as_ref().unwrap();
    match tree_store.get_summary(owner_id, None, root_id).await? {
        Some(node) => Ok(Some(SummaryInput {
            id: node.id,
            content: format!("[{}]\n{}", source_tree.scope, node.content),
            token_count: node.token_count,
            entities: node.entities,
            topics: node.topics,
            time_range_start_ms: node.time_range_start_ms,
            time_range_end_ms: node.time_range_end_ms,
            score: node.score,
        })),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;
    use memory::tree::types::SummaryNode;
    use tempfile::TempDir;

    async fn setup() -> Option<(Pool, std::path::PathBuf, TempDir)> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        let dir = TempDir::new().ok()?;
        Some((pool, dir.path().to_path_buf(), dir))
    }

    #[tokio::test]
    async fn empty_day() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let result = end_of_day_digest(&pool, &content_root, "owner_1", &InertSummariser)
            .await
            .unwrap();
        assert!(matches!(result, DigestOutcome::EmptyDay));
    }

    #[tokio::test]
    async fn digest_creates_global_tree() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree_store = TreeStore::new(pool.clone());

        // Create a source tree with a root summary.
        let source_tree = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:test:sender")
            .await
            .unwrap();

        let summary = SummaryNode {
            id: "sum_test".to_string(),
            tree_id: source_tree.id.clone(),
            user_id: String::new(),
            tree_kind: TreeKind::Source,
            level: 1,
            parent_id: None,
            child_ids: vec!["chunk_1".to_string()],
            content: "Discussion about project Phoenix".to_string(),
            token_count: 50,
            entities: vec!["person:Alice".to_string()],
            topics: vec!["project-phoenix".to_string()],
            time_range_start_ms: 1_700_000_000_000,
            time_range_end_ms: 1_700_000_060_000,
            score: 0.85,
            sealed_at_ms: 1_700_000_120_000,
            deleted: false,
            embedding: None,
        };
        tree_store.insert_summary("owner_1", &summary).await.unwrap();
        tree_store
            .update_tree_after_seal("owner_1", &source_tree.id, 1, 1_700_000_120_000)
            .await
            .unwrap();

        let content_store = ContentStore::new(content_root.to_path_buf());
        content_store.ensure_dirs("owner_1").unwrap();
        let result = end_of_day_digest(&pool, &content_root, "owner_1", &InertSummariser)
            .await
            .unwrap();

        match result {
            DigestOutcome::Emitted { source_count, .. } => {
                assert!(source_count >= 1);
            }
            _ => panic!("expected Emitted, got {:?}", result),
        }

        // Global tree should now exist.
        let global = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Global, GLOBAL_SCOPE)
            .await
            .unwrap();
        assert_eq!(global.kind, TreeKind::Global);
    }
}
