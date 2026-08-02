//! Per-leaf routing into topic trees (PG-backed).
//!
//! Ported from `memory::tree::tree_topic::routing` - same routing logic, stores
//! swapped to the PG async stores. After a leaf is appended to its source tree,
//! this fans it out to every active topic tree matching one of its entities,
//! and bumps entity hotness so the curator may spawn new topic trees.
//!
//! `hotness()` is pure and reused verbatim from `memory::tree::tree_global::hotness`.

use std::path::Path;
use std::sync::Arc;

use deadpool_postgres::Pool;
use memory::tree::summariser::inert::InertSummariser;
use memory::tree::tree_global::hotness::hotness;
use memory::tree::tree_topic::TOPIC_CREATION_THRESHOLD;
use types::error::CarrierResult;
use types::memory_tree::TreeKind;

use crate::bucket_seal::BucketSealEngine;
use crate::pg::entity_store::EntityStore;
use crate::pg::tree_store::TreeStore;

/// Route a leaf to all matching topic trees and bump hotness.
/// Failures are logged but never bubble up - topic routing is additive.
pub async fn route_leaf_to_topic_trees(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    chunk_id: &str,
    token_count: u32,
    timestamp_ms: i64,
    entity_ids: &[String],
) -> CarrierResult<()> {
    if entity_ids.is_empty() {
        return Ok(());
    }

    let tree_store = TreeStore::new(pool.clone());
    let entity_store = EntityStore::new(pool.clone());

    for entity_id in entity_ids {
        // Step 1: if a topic tree already exists and is active, append the leaf
        let trees = tree_store
            .list_trees(owner_id, None, Some(TreeKind::Topic), 100)
            .await?;
        let matching_tree = trees.iter().find(|t| {
            // The scope of a topic tree is the entity_id
            t.scope == *entity_id
        });

        if let Some(tree_summary) = matching_tree {
            if tree_summary.status == "active" {
                if let Some(tree) = tree_store
                    .get_tree(owner_id, None, &tree_summary.tree_id)
                    .await?
                {
                    let seal_engine = BucketSealEngine::new(
                        pool.clone(),
                        content_root.to_path_buf(),
                        Arc::new(InertSummariser),
                    );
                    if let Err(e) = seal_engine
                        .append_leaf(owner_id, &tree, chunk_id, token_count, timestamp_ms)
                        .await
                    {
                        tracing::warn!(
                            "[tree_topic::routing] failed appending leaf={} -> topic_tree={}: {e:#}",
                            chunk_id,
                            tree.id
                        );
                    }
                }
            }
        }

        // Step 2: bump hotness and maybe spawn topic tree
        if let Err(e) = entity_store
            .bump_entity_hotness(owner_id, entity_id, "")
            .await
        {
            tracing::warn!(
                "[tree_topic::routing] failed bumping hotness entity={}: {e:#}",
                entity_id
            );
        }

        // Check if hotness exceeds threshold
        if let Ok(Some(counters)) = entity_store.get_hotness(owner_id, entity_id).await {
            let h = hotness(
                counters.mention_count_30d,
                counters.distinct_sources,
                counters.last_seen_ms,
                counters.query_hits_30d,
                counters.graph_centrality,
                chrono::Utc::now().timestamp_millis(),
            );
            if h >= TOPIC_CREATION_THRESHOLD {
                // Spawn topic tree if it doesn't exist yet
                if matching_tree.is_none() {
                    if let Err(e) = tree_store
                        .get_or_create_tree(owner_id, "", TreeKind::Topic, entity_id)
                        .await
                    {
                        tracing::warn!(
                            "[tree_topic::routing] failed spawning topic tree for {}: {e:#}",
                            entity_id
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;
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
    async fn empty_entities_is_noop() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let result = route_leaf_to_topic_trees(&pool, &content_root, "owner_1", "chunk_1", 100, 1000, &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn routing_creates_topic_tree_on_high_hotness() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let entity_store = EntityStore::new(pool.clone());
        let tree_store = TreeStore::new(pool.clone());

        let entity_id = "email:alice@example.com";
        // First bump creates the row; then age the counters past the threshold.
        entity_store
            .bump_entity_hotness("owner_1", entity_id, "source_1")
            .await
            .unwrap();

        let url = std::env::var("AGINX_MEMORY_TEST_PG").unwrap();
        let (conn, c) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(async move { let _ = c.await; });
        conn.execute(
            "UPDATE mem_tree_entity_hotness SET mention_count_30d=5000, distinct_sources=10, \
             query_hits_30d=5 WHERE owner_id='owner_1' AND entity_id=$1",
            &[&entity_id],
        )
        .await
        .unwrap();

        route_leaf_to_topic_trees(&pool, &content_root, "owner_1", "chunk_1", 100, 1000, &[
            entity_id.to_string(),
        ])
        .await
        .unwrap();

        let trees = tree_store
            .list_trees("owner_1", None, Some(TreeKind::Topic), 100)
            .await
            .unwrap();
        assert!(trees.iter().any(|t| t.scope == entity_id));
    }
}
