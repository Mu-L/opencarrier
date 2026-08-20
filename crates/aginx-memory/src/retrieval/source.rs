//! Per-source summary retrieval (PG-backed).

use deadpool_postgres::Pool;
use types::error::CarrierResult;
use types::memory_tree::{NodeKind, QueryResponse, RetrievalHit, TreeKind, TreeSummary};

use crate::pg::tree_store::TreeStore;
use memory::tree::types::SourceKind;

const DEFAULT_LIMIT: usize = 10;

/// Query source tree summaries.
///
/// When `user_id` is `Some(u)`, only the user's source trees (plus owner-shared)
/// are queried - this is what closes the cross-user leak when no `source_id` is
/// supplied (otherwise every user's source tree under the owner would be read).
pub async fn query_source(
    pool: &Pool,
    owner_id: &str,
    user_id: Option<&str>,
    source_id: Option<&str>,
    source_kind: Option<SourceKind>,
    time_window_days: Option<u32>,
    limit: usize,
) -> CarrierResult<QueryResponse> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let tree_store = TreeStore::new(pool.clone());

    let trees = select_trees(&tree_store, owner_id, user_id, source_id, source_kind).await?;
    let mut hits: Vec<RetrievalHit> = Vec::new();

    for tree in &trees {
        if tree.max_level == 0 {
            continue;
        }
        for level in 1..=tree.max_level {
            let summaries = tree_store
                .list_summaries(owner_id, user_id, &tree.tree_id, Some(level), 100)
                .await?;
            for node in summaries {
                hits.push(RetrievalHit {
                    node_id: node.id,
                    node_kind: NodeKind::Summary,
                    tree_id: tree.tree_id.clone(),
                    tree_kind: TreeKind::Source,
                    tree_scope: tree.scope.clone(),
                    level,
                    content: node.content,
                    entities: node.entities,
                    topics: node.topics,
                    time_range_start_ms: node.time_range_start_ms,
                    time_range_end_ms: node.time_range_end_ms,
                    score: node.score,
                    child_ids: node.child_ids,
                    source_ref: None,
                });
            }
        }
    }

    if let Some(days) = time_window_days {
        hits = filter_by_window(hits, days);
    }

    let total = hits.len();
    hits.sort_by_key(|h| std::cmp::Reverse(h.time_range_end_ms));
    hits.truncate(limit);

    Ok(QueryResponse {
        hits,
        total,
        truncated: total > limit,
    })
}

async fn select_trees(
    tree_store: &TreeStore,
    owner_id: &str,
    user_id: Option<&str>,
    source_id: Option<&str>,
    source_kind: Option<SourceKind>,
) -> CarrierResult<Vec<TreeSummary>> {
    if let Some(id) = source_id {
        let trees = tree_store
            .list_trees(owner_id, user_id, Some(TreeKind::Source), 1000)
            .await?;
        return Ok(trees.into_iter().filter(|t| t.scope == id).collect());
    }
    let all = tree_store
        .list_trees(owner_id, user_id, Some(TreeKind::Source), 1000)
        .await?;
    if let Some(kind) = source_kind {
        let prefix = kind.as_str();
        return Ok(all
            .into_iter()
            .filter(|t| scope_matches_kind(&t.scope, prefix))
            .collect());
    }
    Ok(all)
}

fn scope_matches_kind(scope: &str, kind_prefix: &str) -> bool {
    let lower = scope.to_lowercase();
    if lower.starts_with(&format!("{kind_prefix}:")) {
        return true;
    }
    // Platform-specific prefix mapping
    const PLATFORM_KINDS: &[(&str, &str)] = &[
        ("wechat", "chat"),
        ("feishu", "chat"),
        ("wecom", "chat"),
        ("dingtalk", "chat"),
        ("slack", "chat"),
        ("api", "chat"),
        ("imap", "email"),
        ("gmail", "email"),
        ("outlook", "email"),
        ("notion", "document"),
        ("drive", "document"),
    ];
    PLATFORM_KINDS
        .iter()
        .any(|(platform, kind)| *kind == kind_prefix && lower.starts_with(&format!("{platform}:")))
}

fn filter_by_window(hits: Vec<RetrievalHit>, window_days: u32) -> Vec<RetrievalHit> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_start_ms = now_ms - (window_days as i64 * 86_400_000);
    hits.into_iter()
        .filter(|h| h.time_range_end_ms >= window_start_ms && h.time_range_start_ms <= now_ms)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket_seal::BucketSealEngine;
    use crate::pg::chunk_store::ChunkStore;
    use deadpool_postgres::Manager;
    use memory::tree::summariser::inert::InertSummariser;
    use memory::tree::types::{Chunk, SummaryNode};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn setup() -> Option<(Pool, TempDir)> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .ok()?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr)
            .max_size(4)
            .build()
            .ok()?;
        Some((pool, TempDir::new().ok()?))
    }

    #[tokio::test]
    async fn empty_owner_returns_empty() {
        let (pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let resp = query_source(&pool, "owner_x", None, None, None, None, 10)
            .await
            .unwrap();
        assert!(resp.hits.is_empty());
        assert_eq!(resp.total, 0);
    }

    #[tokio::test]
    async fn query_by_source_id() {
        let (pool, dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree_store = TreeStore::new(pool.clone());
        let chunk_store = ChunkStore::new(pool.clone());

        let tree = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:test:sender")
            .await
            .unwrap();
        for i in 0..10 {
            let mut c = Chunk {
                id: format!("chunk_src_{i}"),
                owner_id: "owner_1".to_string(),
                user_id: String::new(),
                agent_id: "agent_1".to_string(),
                source_kind: SourceKind::Chat,
                source_id: "wechat:test:sender".to_string(),
                source_ref: None,
                timestamp_ms: 1_700_000_000_000,
                time_range_start_ms: 1_700_000_000_000,
                time_range_end_ms: 1_700_000_000_000,
                tags_json: "[]".to_string(),
                content: "test content for source query".to_string(),
                token_count: 6000,
                seq_in_source: i,
                partial_message: false,
                lifecycle_status: "admitted".to_string(),
                created_at_ms: 1_700_000_000_000,
            };
            c.seq_in_source = i;
            chunk_store.upsert_chunks(&[c]).await.unwrap();
        }

        let seal_engine = BucketSealEngine::new(
            pool.clone(),
            dir.path().to_path_buf(),
            Arc::new(InertSummariser),
        );
        for i in 0..10 {
            seal_engine
                .append_to_buffer(
                    "owner_1",
                    &tree.id,
                    0,
                    &format!("chunk_src_{i}"),
                    6000,
                    1_700_000_000_000,
                )
                .await
                .unwrap();
        }
        seal_engine
            .cascade_seals("owner_1", &tree, 0, false)
            .await
            .unwrap();

        let resp = query_source(
            &pool,
            "owner_1",
            None,
            Some("wechat:test:sender"),
            None,
            None,
            10,
        )
        .await
        .unwrap();
        assert!(!resp.hits.is_empty());
        assert_eq!(resp.hits[0].tree_scope, "wechat:test:sender");
    }

    #[test]
    fn scope_matches_kind_pure() {
        assert!(scope_matches_kind("wechat:abc", "chat"));
        assert!(scope_matches_kind("chat:custom", "chat"));
        assert!(!scope_matches_kind("wechat:abc", "email"));
    }

    #[tokio::test]
    async fn query_source_user_isolated() {
        let (pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree_store = TreeStore::new(pool.clone());

        let tree_a = tree_store
            .get_or_create_tree("owner_1", "alice", TreeKind::Source, "wechat:gh:alice")
            .await
            .unwrap();
        let tree_b = tree_store
            .get_or_create_tree("owner_1", "bob", TreeKind::Source, "wechat:gh:bob")
            .await
            .unwrap();

        for (tree, user) in [(&tree_a, "alice"), (&tree_b, "bob")] {
            let node = SummaryNode {
                id: format!("sum_{user}"),
                tree_id: tree.id.clone(),
                user_id: user.to_string(),
                tree_kind: TreeKind::Source,
                level: 1,
                parent_id: None,
                child_ids: vec![format!("chunk_{user}")],
                content: format!("{user}'s conversation summary"),
                token_count: 10,
                entities: vec![],
                topics: vec![],
                time_range_start_ms: 1_700_000_000_000,
                time_range_end_ms: 1_700_000_000_000,
                score: 0.5,
                sealed_at_ms: 1_700_000_000_000,
                deleted: false,
                embedding: None,
            };
            tree_store.insert_summary("owner_1", &node).await.unwrap();
            tree_store
                .update_tree_after_seal("owner_1", &tree.id, 1, 1_700_000_000_000)
                .await
                .unwrap();
        }

        let resp = query_source(&pool, "owner_1", Some("alice"), None, None, None, 10)
            .await
            .unwrap();
        assert!(!resp.hits.is_empty(), "alice should see her own summaries");
        assert!(
            resp.hits.iter().all(|h| h.tree_scope == "wechat:gh:alice"),
            "alice must not see bob's summaries: {:?}",
            resp.hits
        );

        let resp_b = query_source(&pool, "owner_1", Some("bob"), None, None, None, 10)
            .await
            .unwrap();
        assert!(resp_b.hits.iter().all(|h| h.tree_scope == "wechat:gh:bob"));

        let resp_all = query_source(&pool, "owner_1", None, None, None, None, 10)
            .await
            .unwrap();
        assert_eq!(resp_all.hits.len(), 2);
    }
}
