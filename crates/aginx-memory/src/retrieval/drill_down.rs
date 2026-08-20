//! Walk summary children (BFS expansion) - PG-backed.

use std::collections::VecDeque;

use deadpool_postgres::Pool;
use types::error::CarrierResult;
use types::memory_tree::{NodeKind, RetrievalHit, TreeKind};

use crate::pg::chunk_store::ChunkStore;
use crate::pg::tree_store::TreeStore;

/// Drill down from a summary node, returning its children.
/// BFS traversal up to `max_depth` levels.
///
/// When `user_id` is `Some(u)`, only nodes belonging to `u` or owner-shared
/// are traversed.
pub async fn drill_down(
    pool: &Pool,
    owner_id: &str,
    user_id: Option<&str>,
    node_id: &str,
    max_depth: u32,
    limit: Option<usize>,
) -> CarrierResult<Vec<RetrievalHit>> {
    if max_depth == 0 {
        return Ok(Vec::new());
    }

    let tree_store = TreeStore::new(pool.clone());
    let chunk_store = ChunkStore::new(pool.clone());

    // Get the root summary to find its children
    let root = tree_store.get_summary(owner_id, user_id, node_id).await?;
    let (start_children, root_tree_id) = match &root {
        Some(s) => (s.child_ids.clone(), Some(s.tree_id.clone())),
        None => {
            // It's a leaf - no children
            if chunk_store
                .get_chunk(owner_id, user_id, node_id)
                .await?
                .is_some()
            {
                return Ok(Vec::new());
            }
            return Ok(Vec::new());
        }
    };

    // Resolve the root summary's tree scope (await can't live in a sync
    // closure, so compute it from the captured tree_id).
    let root_tree_scope = match root_tree_id {
        Some(tid) => tree_store
            .get_tree(owner_id, user_id, &tid)
            .await
            .ok()
            .flatten()
            .map(|t| t.scope)
            .unwrap_or_default(),
        None => String::new(),
    };

    let mut hits: Vec<RetrievalHit> = Vec::new();
    let mut frontier: VecDeque<(String, u32)> =
        start_children.into_iter().map(|id| (id, 1u32)).collect();

    while let Some((id, depth)) = frontier.pop_front() {
        if depth > max_depth {
            continue;
        }

        // Try as summary
        if let Some(node) = tree_store.get_summary(owner_id, user_id, &id).await? {
            let scope = tree_store
                .get_tree(owner_id, user_id, &node.tree_id)
                .await?
                .map(|t| t.scope)
                .unwrap_or_else(|| root_tree_scope.clone());
            let child_ids = node.child_ids.clone();
            hits.push(RetrievalHit {
                node_id: node.id,
                node_kind: NodeKind::Summary,
                tree_id: node.tree_id,
                tree_kind: node.tree_kind,
                tree_scope: scope,
                level: node.level,
                content: node.content,
                entities: node.entities,
                topics: node.topics,
                time_range_start_ms: node.time_range_start_ms,
                time_range_end_ms: node.time_range_end_ms,
                score: node.score,
                child_ids: node.child_ids,
                source_ref: None,
            });
            if depth < max_depth {
                for next in child_ids {
                    frontier.push_back((next, depth + 1));
                }
            }
            continue;
        }

        // Try as chunk (leaf)
        if let Some(chunk) = chunk_store.get_chunk(owner_id, user_id, &id).await? {
            hits.push(RetrievalHit {
                node_id: chunk.id,
                node_kind: NodeKind::Leaf,
                tree_id: String::new(),
                tree_kind: TreeKind::Source,
                tree_scope: chunk.source_id.clone(),
                level: 0,
                content: chunk.content,
                entities: Vec::new(),
                topics: Vec::new(),
                time_range_start_ms: chunk.time_range_start_ms,
                time_range_end_ms: chunk.time_range_end_ms,
                score: 0.0,
                child_ids: Vec::new(),
                source_ref: chunk.source_ref,
            });
        }
    }

    if let Some(n) = limit {
        hits.truncate(n);
    }

    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket_seal::BucketSealEngine;
    use deadpool_postgres::Manager;
    use memory::tree::summariser::inert::InertSummariser;
    use memory::tree::types::{Chunk, SourceKind};
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
    async fn depth_zero_returns_empty() {
        let (pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let result = drill_down(&pool, "owner_1", None, "nonexistent", 0, None)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn invalid_id_returns_empty() {
        let (pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let result = drill_down(&pool, "owner_1", None, "nonexistent", 1, None)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn drill_from_sealed_tree() {
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
                id: format!("chunk_dd_{i}"),
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
                content: "test content for drill down".to_string(),
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
                    &format!("chunk_dd_{i}"),
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

        let refreshed = tree_store
            .get_tree("owner_1", None, &tree.id)
            .await
            .unwrap()
            .unwrap();
        let root_id = refreshed.root_id.unwrap();

        let result = drill_down(&pool, "owner_1", None, &root_id, 1, None)
            .await
            .unwrap();
        assert!(
            !result.is_empty(),
            "drill_down from sealed L1 should return leaf children"
        );
    }
}
