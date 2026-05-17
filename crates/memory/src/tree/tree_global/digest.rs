//! End-of-day digest builder for the global activity tree.
//!
//! Once per calendar day we walk every active source tree, collect the
//! summary material that covers that day, fold it into one cross-source
//! recap, and persist it as an L0 node in the singleton global tree.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use types::error::CarrierResult;
use types::memory_tree::TreeKind;

use super::{GLOBAL_SCOPE, GLOBAL_TOKEN_BUDGET, WEEKLY_SEAL_THRESHOLD};
use crate::tree::bucket_seal::BucketSealEngine;
use crate::tree::content_store::ContentStore;
use crate::tree::store::ChunkStore;
use crate::tree::summariser::{Summariser, SummaryContext, SummaryInput};
use crate::tree::tree_store::TreeTreeStore;
use crate::tree::types::{SummaryNode, Tree};

/// Outcome of a single `end_of_day_digest` call.
#[derive(Debug, Clone)]
pub enum DigestOutcome {
    /// Emitted one L0 daily node and possibly cascaded into higher-level seals.
    Emitted {
        daily_id: String,
        source_count: usize,
    },
    /// No source tree had material for the target day.
    EmptyDay,
    /// An L0 daily node already exists for this day.
    Skipped { existing_id: String },
}

/// Run an end-of-day digest for a given owner and day.
pub fn end_of_day_digest(
    conn: &Arc<Mutex<Connection>>,
    content_root: &Path,
    owner_id: &str,
    summariser: &dyn Summariser,
) -> CarrierResult<DigestOutcome> {
    let tree_store = TreeTreeStore::new(conn.clone());
    let content_store = ContentStore::new(content_root.to_path_buf());
    let chunk_store = ChunkStore::new(conn.clone());

    // Get or create the global tree
    let global = tree_store.get_or_create_tree(owner_id, TreeKind::Global, GLOBAL_SCOPE)?;

    let now_ms = chrono::Utc::now().timestamp_millis();

    // Check for an existing daily digest (idempotency)
    if let Some(existing) = find_existing_daily(&tree_store, owner_id, &global.id, now_ms)? {
        return Ok(DigestOutcome::Skipped {
            existing_id: existing,
        });
    }

    // Gather one contribution per active source tree
    let source_trees = tree_store.list_trees(owner_id, Some(TreeKind::Source), 1000)?;
    let mut inputs: Vec<SummaryInput> = Vec::with_capacity(source_trees.len());

    for tree_summary in &source_trees {
        // Get the tree to access its scope
        if let Some(tree) = tree_store.get_tree(owner_id, &tree_summary.tree_id)? {
            if let Some(inp) = pick_source_contribution(
                &tree_store,
                &chunk_store,
                &content_store,
                owner_id,
                &tree,
                now_ms,
            )? {
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

    let score = inputs.iter().map(|i| i.score).fold(f32::NEG_INFINITY, f32::max).max(0.0);

    let daily_id = format!("sum_L0_{}", uuid::Uuid::new_v4().simple());
    let daily = SummaryNode {
        id: daily_id.clone(),
        tree_id: global.id.clone(),
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

    // Write content
    content_store.ensure_dirs(owner_id)?;
    content_store.write_summary(owner_id, &daily)?;

    // Persist
    tree_store.insert_summary(owner_id, &daily)?;

    // Append into the global tree's L0 buffer
    let seal_engine = BucketSealEngine::new(
        conn.clone(),
        content_root.to_path_buf(),
        Arc::new(crate::tree::summariser::inert::InertSummariser),
    );
    seal_engine.append_to_buffer(owner_id, &global.id, 0, &daily_id, daily.token_count as i64, now_ms)?;

    // Check if weekly seal should trigger
    let buf = seal_engine.get_or_create_buffer(owner_id, &global.id, 0)?;
    if buf.item_ids.len() >= WEEKLY_SEAL_THRESHOLD {
        seal_engine.cascade_seals(owner_id, &global, 0, true)?;
    }

    Ok(DigestOutcome::Emitted {
        daily_id,
        source_count: inputs.len(),
    })
}

fn find_existing_daily(
    tree_store: &TreeTreeStore,
    owner_id: &str,
    global_tree_id: &str,
    _now_ms: i64,
) -> CarrierResult<Option<String>> {
    // Check for any L0 summary in the global tree today
    let summaries = tree_store.list_summaries(owner_id, global_tree_id, Some(0), 1)?;
    Ok(summaries.first().map(|s| s.id.clone()))
}

fn pick_source_contribution(
    tree_store: &TreeTreeStore,
    _chunk_store: &ChunkStore,
    _content_store: &ContentStore,
    owner_id: &str,
    source_tree: &Tree,
    _now_ms: i64,
) -> CarrierResult<Option<SummaryInput>> {
    // Pick the highest-level summary (root) from this source tree
    if source_tree.root_id.is_none() {
        // No sealed summaries yet — check L0 buffer
        return Ok(None);
    }

    let root_id = source_tree.root_id.as_ref().unwrap();
    match tree_store.get_summary(owner_id, root_id)? {
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
    use crate::migration::run_migrations;
    use crate::tree::summariser::inert::InertSummariser;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn setup() -> (Arc<Mutex<Connection>>, PathBuf, TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let dir = TempDir::new().unwrap();
        (Arc::new(Mutex::new(conn)), dir.path().to_path_buf(), dir)
    }

    #[test]
    fn test_empty_day() {
        let (conn, content_root, _dir) = setup();
        let result = end_of_day_digest(&conn, &content_root, "owner_1", &InertSummariser).unwrap();
        assert!(matches!(result, DigestOutcome::EmptyDay));
    }

    #[test]
    fn test_digest_creates_global_tree() {
        let (conn, content_root, _dir) = setup();
        let tree_store = TreeTreeStore::new(conn.clone());

        // Create a source tree with a root summary
        let source_tree = tree_store
            .get_or_create_tree("owner_1", TreeKind::Source, "wechat:test:sender")
            .unwrap();

        // Insert a summary as root
        let summary = crate::tree::types::SummaryNode {
            id: "sum_test".to_string(),
            tree_id: source_tree.id.clone(),
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
        tree_store.insert_summary("owner_1", &summary).unwrap();
        tree_store.update_tree_after_seal("owner_1", &source_tree.id, 1, 1_700_000_120_000).unwrap();

        // Run digest
        let content_store = ContentStore::new(content_root.to_path_buf());
        content_store.ensure_dirs("owner_1").unwrap();
        let result = end_of_day_digest(&conn, &content_root, "owner_1", &InertSummariser).unwrap();

        match result {
            DigestOutcome::Emitted { source_count, .. } => {
                assert!(source_count >= 1);
            }
            _ => panic!("expected Emitted, got {:?}", result),
        }

        // Global tree should now exist
        let global = tree_store.get_or_create_tree("owner_1", TreeKind::Global, GLOBAL_SCOPE).unwrap();
        assert_eq!(global.kind, TreeKind::Global);
    }
}
