//! Per-leaf routing into topic trees.
//!
//! After a leaf is appended to its source tree, this module fans it out to
//! every active topic tree matching one of its entities. Also bumps entity
//! hotness counters so the curator may spawn new topic trees.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use types::error::CarrierResult;
use types::memory_tree::TreeKind;

use super::TOPIC_CREATION_THRESHOLD;
use crate::tree::bucket_seal::BucketSealEngine;
use crate::tree::entity_store::EntityStore;
use crate::tree::summariser::inert::InertSummariser;
use crate::tree::tree_store::TreeTreeStore;

/// Route a leaf to all matching topic trees and bump hotness.
/// Failures are logged but never bubble up — topic routing is additive.
pub fn route_leaf_to_topic_trees(
    conn: &Arc<Mutex<Connection>>,
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

    let tree_store = TreeTreeStore::new(conn.clone());
    let entity_store = EntityStore::new(conn.clone());

    for entity_id in entity_ids {
        // Step 1: if a topic tree already exists and is active, append the leaf
        let trees = tree_store.list_trees(owner_id, Some(TreeKind::Topic), 100)?;
        let matching_tree = trees.iter().find(|t| {
            // The scope of a topic tree is the entity_id
            t.scope == *entity_id
        });

        if let Some(tree_summary) = matching_tree {
            if tree_summary.status == "active" {
                if let Some(tree) = tree_store.get_tree(owner_id, &tree_summary.tree_id)? {
                    let seal_engine = BucketSealEngine::new(
                        conn.clone(),
                        content_root.to_path_buf(),
                        Arc::new(InertSummariser),
                    );
                    if let Err(e) = seal_engine.append_leaf(owner_id, &tree, chunk_id, token_count, timestamp_ms) {
                        tracing::warn!(
                            "[tree_topic::routing] failed appending leaf={} → topic_tree={}: {e:#}",
                            chunk_id,
                            tree.id
                        );
                    }
                }
            }
        }

        // Step 2: bump hotness and maybe spawn topic tree
        if let Err(e) = entity_store.bump_entity_hotness(owner_id, entity_id, "") {
            tracing::warn!(
                "[tree_topic::routing] failed bumping hotness entity={}: {e:#}",
                entity_id
            );
        }

        // Check if hotness exceeds threshold
        if let Ok(Some(counters)) = entity_store.get_hotness(owner_id, entity_id) {
            let hotness = crate::tree::tree_global::hotness::hotness(
                counters.mention_count_30d,
                counters.distinct_sources,
                counters.last_seen_ms,
                counters.query_hits_30d,
                counters.graph_centrality,
                chrono::Utc::now().timestamp_millis(),
            );
            if hotness >= TOPIC_CREATION_THRESHOLD {
                // Spawn topic tree if it doesn't exist yet
                if matching_tree.is_none() {
                    if let Err(e) = tree_store.get_or_create_tree(owner_id, TreeKind::Topic, entity_id) {
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
    use crate::migration::run_migrations;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn setup() -> (Arc<Mutex<Connection>>, PathBuf, TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let dir = TempDir::new().unwrap();
        (Arc::new(Mutex::new(conn)), dir.path().to_path_buf(), dir)
    }

    #[test]
    fn test_empty_entities_is_noop() {
        let (conn, content_root, _dir) = setup();
        let result = route_leaf_to_topic_trees(
            &conn,
            &content_root,
            "owner_1",
            "chunk_1",
            100,
            1000,
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_routing_creates_topic_tree_on_high_hotness() {
        let (conn, content_root, _dir) = setup();
        let entity_store = EntityStore::new(conn.clone());
        let tree_store = TreeTreeStore::new(conn.clone());

        // Seed high hotness by bumping with high mention count
        let entity_id = "email:alice@example.com";
        // First bump creates the row, then we manually set high counters
        entity_store.bump_entity_hotness("owner_1", entity_id, "source_1").unwrap();

        // Directly update hotness counters to cross the threshold
        {
            let c = conn.lock().unwrap();
            c.execute(
                "UPDATE mem_tree_entity_hotness SET mention_count_30d = 5000, distinct_sources = 10, query_hits_30d = 5
                 WHERE owner_id = 'owner_1' AND entity_id = ?1",
                rusqlite::params![entity_id],
            ).unwrap();
        }

        // Route a leaf
        route_leaf_to_topic_trees(
            &conn,
            &content_root,
            "owner_1",
            "chunk_1",
            100,
            1000,
            &[entity_id.to_string()],
        ).unwrap();

        // Check topic tree was created
        let trees = tree_store.list_trees("owner_1", Some(TreeKind::Topic), 100).unwrap();
        assert!(trees.iter().any(|t| t.scope == entity_id));
    }
}
