//! Batch chunk hydration - fetch leaf chunks by their IDs directly (PG-backed).

use deadpool_postgres::Pool;
use types::error::CarrierResult;
use types::memory_tree::{NodeKind, QueryResponse, RetrievalHit, TreeKind};

use crate::pg::chunk_store::ChunkStore;
use crate::pg::score_store::ScoreStore;

/// Maximum number of chunk IDs that can be fetched in one call.
const MAX_FETCH_BATCH: usize = 20;
const DEFAULT_LIMIT: usize = 20;

/// Fetch leaf chunks by their IDs directly (no BFS traversal).
///
/// When `user_id` is `Some(u)`, only chunks belonging to `u` or owner-shared
/// are returned - this prevents one user from hydrating another's chunk by id.
/// Missing IDs are silently skipped (best-effort). Results are sorted
/// oldest-first by `time_range_start_ms`.
pub async fn fetch_leaves(
    pool: &Pool,
    owner_id: &str,
    user_id: Option<&str>,
    chunk_ids: &[String],
    limit: usize,
) -> CarrierResult<QueryResponse> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let cap = limit.min(MAX_FETCH_BATCH);
    let chunk_store = ChunkStore::new(pool.clone());
    let score_store = ScoreStore::new(pool.clone());

    let requested = chunk_ids.len();
    let mut hits: Vec<RetrievalHit> = Vec::new();

    for id in chunk_ids.iter().take(cap) {
        if let Some(chunk) = chunk_store.get_chunk(owner_id, user_id, id).await? {
            let score = score_store
                .get_score(owner_id, id)
                .await
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
    use deadpool_postgres::Manager;
    use memory::tree::types::{Chunk, SourceKind};

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

    async fn insert_chunk(pool: &Pool, owner_id: &str, id: &str, seq: u32, content: &str, user_id: &str) {
        let chunk = Chunk {
            id: id.to_string(),
            owner_id: owner_id.to_string(),
            user_id: user_id.to_string(),
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
        ChunkStore::new(pool.clone()).upsert_chunks(&[chunk]).await.unwrap();
    }

    #[tokio::test]
    async fn empty_ids_returns_empty() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let resp = fetch_leaves(&pool, "owner_1", None, &[], 10).await.unwrap();
        assert!(resp.hits.is_empty());
    }

    #[tokio::test]
    async fn missing_ids_skipped() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let resp = fetch_leaves(&pool, "owner_1", None, &["nonexistent".to_string()], 10)
            .await
            .unwrap();
        assert!(resp.hits.is_empty());
    }

    #[tokio::test]
    async fn direct_chunk_lookup() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip");
                return;
            }
        };
        insert_chunk(&pool, "owner_1", "chunk_1", 0, "hello world", "").await;
        let resp = fetch_leaves(&pool, "owner_1", None, &["chunk_1".to_string()], 10)
            .await
            .unwrap();
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].node_kind, NodeKind::Leaf);
        assert_eq!(resp.hits[0].content, "hello world");
    }

    #[tokio::test]
    async fn batch_lookup_sorted_oldest_first() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip");
                return;
            }
        };
        insert_chunk(&pool, "owner_1", "chunk_b", 1, "second", "").await;
        insert_chunk(&pool, "owner_1", "chunk_a", 0, "first", "").await;
        let resp = fetch_leaves(
            &pool,
            "owner_1",
            None,
            &["chunk_b".to_string(), "chunk_a".to_string()],
            10,
        )
        .await
        .unwrap();
        assert_eq!(resp.hits.len(), 2);
        assert_eq!(resp.hits[0].content, "first");
        assert_eq!(resp.hits[1].content, "second");
    }

    #[tokio::test]
    async fn batch_cap_and_truncation() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let ids: Vec<String> = (0..25).map(|i| format!("chunk_cap_{i}")).collect();
        for i in 0..25 {
            insert_chunk(&pool, "owner_1", &format!("chunk_cap_{i}"), i, "content", "").await;
        }
        let resp = fetch_leaves(&pool, "owner_1", None, &ids, 100).await.unwrap();
        assert!(resp.hits.len() <= MAX_FETCH_BATCH);
        assert!(resp.truncated);
    }

    #[tokio::test]
    async fn mixed_found_and_missing() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip");
                return;
            }
        };
        insert_chunk(&pool, "owner_1", "chunk_exists", 0, "found", "").await;
        let resp = fetch_leaves(
            &pool,
            "owner_1",
            None,
            &["chunk_exists".to_string(), "missing".to_string()],
            10,
        )
        .await
        .unwrap();
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].content, "found");
    }

    /// One user must not hydrate another user's chunk by guessing its id.
    #[tokio::test]
    async fn fetch_leaves_user_isolation() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip");
                return;
            }
        };
        insert_chunk(&pool, "owner_1", "chunk_alice", 0, "alice's secret", "alice").await;
        insert_chunk(&pool, "owner_1", "chunk_shared", 0, "shared", "").await;

        // bob requests both ids - only the owner-shared one is returned.
        let resp = fetch_leaves(
            &pool,
            "owner_1",
            Some("bob"),
            &["chunk_alice".to_string(), "chunk_shared".to_string()],
            10,
        )
        .await
        .unwrap();
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].node_id, "chunk_shared");
    }
}
