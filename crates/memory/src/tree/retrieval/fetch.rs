//! Batch chunk hydration — fetch all leaf chunks under a summary node.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use types::error::CarrierResult;
use types::memory_tree::{NodeKind, QueryResponse, RetrievalHit, TreeKind};

use crate::tree::store::ChunkStore;
use crate::tree::tree_store::TreeTreeStore;

const DEFAULT_LIMIT: usize = 20;

/// Fetch all leaf (chunk) nodes under a summary, using BFS to collect child_ids.
pub fn fetch_leaves(
    conn: &Arc<Mutex<Connection>>,
    owner_id: &str,
    node_id: &str,
    limit: usize,
) -> CarrierResult<QueryResponse> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let tree_store = TreeTreeStore::new(conn.clone());
    let chunk_store = ChunkStore::new(conn.clone());

    // If the node itself is a leaf chunk, return it directly.
    if let Some(chunk) = chunk_store.get_chunk(owner_id, node_id)? {
        let hit = RetrievalHit {
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
        };
        return Ok(QueryResponse {
            hits: vec![hit],
            total: 1,
            truncated: false,
        });
    }

    // Otherwise, BFS from the summary node, collecting leaf chunks.
    let root = tree_store.get_summary(owner_id, node_id)?;
    let start_children: Vec<String> = match &root {
        Some(s) => s.child_ids.clone(),
        None => return Ok(QueryResponse {
            hits: Vec::new(),
            total: 0,
            truncated: false,
        }),
    };

    let mut hits: Vec<RetrievalHit> = Vec::new();
    let mut frontier: VecDeque<String> = start_children.into_iter().collect();

    while let Some(id) = frontier.pop_front() {
        if hits.len() >= limit {
            break;
        }

        // Try as summary — enqueue its children.
        if let Some(node) = tree_store.get_summary(owner_id, &id)? {
            for child in &node.child_ids {
                frontier.push_back(child.clone());
            }
            continue;
        }

        // Try as chunk (leaf).
        if let Some(chunk) = chunk_store.get_chunk(owner_id, &id)? {
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

    let total = hits.len();
    let truncated = total > limit;
    hits.truncate(limit);

    // Sort oldest-first (by time_range_start_ms) for chronological reading.
    hits.sort_by_key(|h| h.time_range_start_ms);

    Ok(QueryResponse {
        hits,
        total,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use crate::tree::bucket_seal::BucketSealEngine;
    use crate::tree::summariser::inert::InertSummariser;
    use crate::tree::types::{Chunk, SourceKind};
    use crate::tree::store::ChunkStore;
    use tempfile::TempDir;

    fn setup() -> (Arc<Mutex<Connection>>, TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let dir = TempDir::new().unwrap();
        (Arc::new(Mutex::new(conn)), dir)
    }

    #[test]
    fn test_unknown_node_returns_empty() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let resp = fetch_leaves(&conn, "owner_1", "nonexistent", 10)?;
        assert!(resp.hits.is_empty());
        Ok(())
    }

    #[test]
    fn test_direct_chunk_returns_itself() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let chunk_store = ChunkStore::new(conn.clone());

        let chunk = Chunk {
            id: "chunk_fl_1".to_string(),
            owner_id: "owner_1".to_string(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:test:sender".to_string(),
            source_ref: Some("ref:123".to_string()),
            timestamp_ms: 1_700_000_000_000,
            time_range_start_ms: 1_700_000_000_000,
            time_range_end_ms: 1_700_000_000_000,
            tags_json: "[]".to_string(),
            content: "direct leaf content".to_string(),
            token_count: 5,
            seq_in_source: 0,
            partial_message: false,
            lifecycle_status: "admitted".to_string(),
            created_at_ms: 1_700_000_000_000,
        };
        chunk_store.upsert_chunks(&[chunk])?;

        let resp = fetch_leaves(&conn, "owner_1", "chunk_fl_1", 10)?;
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].node_kind, NodeKind::Leaf);
        assert_eq!(resp.hits[0].content, "direct leaf content");
        assert_eq!(resp.hits[0].source_ref.as_deref(), Some("ref:123"));
        Ok(())
    }

    #[test]
    fn test_fetch_from_sealed_tree() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let tree_store = TreeTreeStore::new(conn.clone());
        let chunk_store = ChunkStore::new(conn.clone());

        let tree = tree_store.get_or_create_tree("owner_1", TreeKind::Source, "wechat:test:sender")?;

        for i in 0..10 {
            let chunk = Chunk {
                id: format!("chunk_fl_seal_{i}"),
                owner_id: "owner_1".to_string(),
                agent_id: "agent_1".to_string(),
                source_kind: SourceKind::Chat,
                source_id: "wechat:test:sender".to_string(),
                source_ref: None,
                timestamp_ms: 1_700_000_000_000 + i as i64 * 1000,
                time_range_start_ms: 1_700_000_000_000 + i as i64 * 1000,
                time_range_end_ms: 1_700_000_000_000 + i as i64 * 1000,
                tags_json: "[]".to_string(),
                content: format!("leaf content {i}"),
                token_count: 6000,
                seq_in_source: i,
                partial_message: false,
                lifecycle_status: "admitted".to_string(),
                created_at_ms: 1_700_000_000_000,
            };
            chunk_store.upsert_chunks(&[chunk])?;
        }

        let seal_engine = BucketSealEngine::new(conn.clone(), _dir.path().to_path_buf(), Arc::new(InertSummariser));
        for i in 0..10 {
            seal_engine.append_to_buffer("owner_1", &tree.id, 0, &format!("chunk_fl_seal_{i}"), 6000, 1_700_000_000_000)?;
        }
        seal_engine.cascade_seals("owner_1", &tree, 0, false)?;

        let refreshed = tree_store.get_tree("owner_1", &tree.id)?.unwrap();
        let root_id = refreshed.root_id.unwrap();

        let resp = fetch_leaves(&conn, "owner_1", &root_id, 100)?;
        assert!(!resp.hits.is_empty(), "fetch_leaves from sealed root should return leaf chunks");
        assert_eq!(resp.hits[0].node_kind, NodeKind::Leaf);
        Ok(())
    }
}
