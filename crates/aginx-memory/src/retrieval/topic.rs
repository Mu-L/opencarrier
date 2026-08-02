//! Entity-scoped retrieval across topic trees and entity index (PG-backed).

use deadpool_postgres::Pool;
use types::error::CarrierResult;
use types::memory_tree::{NodeKind, QueryResponse, RetrievalHit, TreeKind};

use crate::pg::chunk_store::ChunkStore;
use crate::pg::entity_store::EntityStore;
use crate::pg::tree_store::TreeStore;

const DEFAULT_LIMIT: usize = 10;

/// Query by entity id - returns hits from the entity index plus topic tree root.
///
/// When `user_id` is `Some(u)`, only the user's mentions (or owner-shared) are
/// returned. Topic trees themselves are owner-scoped; this filters at the
/// chunk/summary read layer.
pub async fn query_topic(
    pool: &Pool,
    owner_id: &str,
    user_id: Option<&str>,
    entity_id: &str,
    time_window_days: Option<u32>,
    limit: usize,
) -> CarrierResult<QueryResponse> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let entity_store = EntityStore::new(pool.clone());
    let tree_store = TreeStore::new(pool.clone());
    let chunk_store = ChunkStore::new(pool.clone());

    let mut hits: Vec<RetrievalHit> = Vec::new();

    // 1. Topic tree root summary (if exists)
    let topic_trees = tree_store
        .list_trees(owner_id, user_id, Some(TreeKind::Topic), 1000)
        .await?;
    if let Some(topic_tree) = topic_trees.iter().find(|t| t.scope == entity_id) {
        if let Some(full_tree) = tree_store
            .get_tree(owner_id, user_id, &topic_tree.tree_id)
            .await?
        {
            if let Some(root_id) = &full_tree.root_id {
                if let Some(node) = tree_store.get_summary(owner_id, user_id, root_id).await? {
                    hits.push(RetrievalHit {
                        node_id: node.id,
                        node_kind: NodeKind::Summary,
                        tree_id: topic_tree.tree_id.clone(),
                        tree_kind: TreeKind::Topic,
                        tree_scope: topic_tree.scope.clone(),
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
                }
            }
        }
    }

    // 2. Entity index hits
    let index_rows = entity_store
        .chunks_for_entity(owner_id, user_id, entity_id, 100)
        .await?;
    for (node_id, _node_kind) in &index_rows {
        // Try as summary first
        if let Some(node) = tree_store.get_summary(owner_id, user_id, node_id).await? {
            let scope = tree_store
                .get_tree(owner_id, user_id, &node.tree_id)
                .await?
                .map(|t| t.scope)
                .unwrap_or_default();
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
            continue;
        }
        // Try as chunk (leaf)
        if let Some(chunk) = chunk_store.get_chunk(owner_id, user_id, node_id).await? {
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

    // Deduplicate by node_id
    let mut seen = std::collections::BTreeSet::new();
    hits.retain(|h| seen.insert(h.node_id.clone()));

    if let Some(days) = time_window_days {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let window_start_ms = now_ms - (days as i64 * 86_400_000);
        hits.retain(|h| h.time_range_end_ms >= window_start_ms && h.time_range_start_ms <= now_ms);
    }

    let total = hits.len();
    hits.sort_by(|a, b| b.time_range_end_ms.cmp(&a.time_range_end_ms));
    hits.truncate(limit);

    Ok(QueryResponse {
        hits,
        total,
        truncated: total > limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;

    async fn setup() -> Option<Pool> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()
    }

    #[tokio::test]
    async fn unknown_entity_returns_empty() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let resp = query_topic(&pool, "owner_1", None, "email:nobody@example.com", None, 10)
            .await
            .unwrap();
        assert!(resp.hits.is_empty());
    }
}
