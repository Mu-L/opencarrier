//! Batch chunk hydration — fetch leaf chunks by their IDs directly.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use types::error::CarrierResult;
use types::memory_tree::{NodeKind, QueryResponse, RetrievalHit, TreeKind};

use crate::tree::store::ChunkStore;
use crate::tree::score_store::ScoreStore;

/// Maximum number of chunk IDs that can be fetched in one call.
const MAX_FETCH_BATCH: usize = 20;
const DEFAULT_LIMIT: usize = 20;

/// Fetch leaf chunks by their IDs directly (no BFS traversal).
///
/// Missing IDs are silently skipped (best-effort). Results are sorted
/// oldest-first by `time_range_start_ms`.
pub fn fetch_leaves(
    conn: &Arc<Mutex<Connection>>,
    owner_id: &str,
    chunk_ids: &[String],
    limit: usize,
) -> CarrierResult<QueryResponse> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let cap = limit.min(MAX_FETCH_BATCH);
    let chunk_store = ChunkStore::new(conn.clone());
    let score_store = ScoreStore::new(conn.clone());

    let requested = chunk_ids.len();
    let mut hits: Vec<RetrievalHit> = Vec::new();

    for id in chunk_ids.iter().take(cap) {
        if let Some(chunk) = chunk_store.get_chunk(owner_id, id)? {
            let score = score_store
                .get_score(owner_id, id)
                .ok()
                .flatten()
                .map(|s| s.total)
                .unwrap_or(0.0);

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
                score,
                child_ids: Vec::new(),
                source_ref: chunk.source_ref,
            });
        }
        // Missing chunk: skip silently (best-effort)
    }

    let truncated = requested > cap;
    hits.sort_by_key(|h| h.time_range_start_ms);

    Ok(QueryResponse {
        hits,
        total: requested,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use crate::tree::types::{Chunk, SourceKind};
    use tempfile::TempDir;

    fn setup() -> (Arc<Mutex<Connection>>, TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let dir = TempDir::new().unwrap();
        (Arc::new(Mutex::new(conn)), dir)
    }

    fn insert_chunk(conn: &Arc<Mutex<Connection>>, owner_id: &str, id: &str, seq: u32, content: &str) {
        let store = ChunkStore::new(conn.clone());
        let chunk = Chunk {
            id: id.to_string(),
            owner_id: owner_id.to_string(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:test:sender".to_string(),
            source_ref: None,
            timestamp_ms: 1_700_000_000_000 + seq as i64 * 1000,
            time_range_start_ms: 1_700_000_000_000 + seq as i64 * 1000,
            time_range_end_ms: 1_700_000_000_000 + seq as i64 * 1000,
            tags_json: "[]".to_string(),
            content: content.to_string(),
            token_count: 10,
            seq_in_source: seq,
            partial_message: false,
            lifecycle_status: "admitted".to_string(),
            created_at_ms: 1_700_000_000_000,
        };
        store.upsert_chunks(&[chunk]).unwrap();
    }

    #[test]
    fn test_empty_ids_returns_empty() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let resp = fetch_leaves(&conn, "owner_1", &[], 10)?;
        assert!(resp.hits.is_empty());
        Ok(())
    }

    #[test]
    fn test_missing_ids_skipped() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let resp = fetch_leaves(&conn, "owner_1", &["nonexistent".to_string()], 10)?;
        assert!(resp.hits.is_empty());
        Ok(())
    }

    #[test]
    fn test_direct_chunk_lookup() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        insert_chunk(&conn, "owner_1", "chunk_1", 0, "hello world");

        let resp = fetch_leaves(&conn, "owner_1", &["chunk_1".to_string()], 10)?;
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].node_kind, NodeKind::Leaf);
        assert_eq!(resp.hits[0].content, "hello world");
        Ok(())
    }

    #[test]
    fn test_batch_lookup_sorted_oldest_first() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        insert_chunk(&conn, "owner_1", "chunk_b", 1, "second");
        insert_chunk(&conn, "owner_1", "chunk_a", 0, "first");

        let resp = fetch_leaves(
            &conn,
            "owner_1",
            &["chunk_b".to_string(), "chunk_a".to_string()],
            10,
        )?;
        assert_eq!(resp.hits.len(), 2);
        assert_eq!(resp.hits[0].content, "first");
        assert_eq!(resp.hits[1].content, "second");
        Ok(())
    }

    #[test]
    fn test_batch_cap_and_truncation() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let ids: Vec<String> = (0..25).map(|i| format!("chunk_cap_{i}")).collect();
        for i in 0..25 {
            insert_chunk(&conn, "owner_1", &format!("chunk_cap_{i}"), i, "content");
        }

        let resp = fetch_leaves(&conn, "owner_1", &ids, 100)?;
        assert!(resp.hits.len() <= MAX_FETCH_BATCH);
        assert!(resp.truncated);
        Ok(())
    }

    #[test]
    fn test_mixed_found_and_missing() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        insert_chunk(&conn, "owner_1", "chunk_exists", 0, "found");

        let resp = fetch_leaves(
            &conn,
            "owner_1",
            &["chunk_exists".to_string(), "missing".to_string()],
            10,
        )?;
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].content, "found");
        Ok(())
    }
}
