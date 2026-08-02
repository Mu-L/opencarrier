//! Append + cascade-seal for summary trees (PG-backed).
//!
//! Ported from `memory::tree::bucket_seal` - same seal-cascade logic, but the
//! stores are the PG async stores (`crate::pg::tree_store::TreeStore`,
//! `crate::pg::chunk_store::ChunkStore`) so every method is `async`. The
//! `content_store` (file-backed, Obsidian-compatible) and the `Summariser`
//! trait are reused verbatim from the memory crate - they're not DB-backed.
//!
//! Seal gates differ by level:
//!
//! - **L0 (leaves -> L1)**: seal when `token_sum >= INPUT_TOKEN_BUDGET` OR
//!   `item_ids.len() >= SUMMARY_FANOUT`.
//! - **L≥1 (summaries -> next level)**: seal when `item_ids.len() >= SUMMARY_FANOUT`.
//!
//! When a buffer seals, its items move into the new summary's `child_ids`,
//! the buffer clears, and the new summary id is queued at the next level.
//! The cascade continues upward until a buffer fails its gate.

use std::path::PathBuf;
use std::sync::Arc;

use deadpool_postgres::Pool;
use memory::tree::content_store::ContentStore;
use memory::tree::summariser::{Summariser, SummaryContext, SummaryInput};
use memory::tree::types::{
    Buffer, SummaryNode, Tree, INPUT_TOKEN_BUDGET, MAX_CASCADE_DEPTH, OUTPUT_TOKEN_BUDGET,
    SUMMARY_FANOUT,
};
use types::error::{CarrierError, CarrierResult};

use crate::pg::chunk_store::ChunkStore;
use crate::pg::tree_store::TreeStore;

/// Bucket seal engine - drives append + cascade-seal operations.
pub struct BucketSealEngine {
    tree_store: TreeStore,
    chunk_store: ChunkStore,
    content_store: ContentStore,
    summariser: Arc<dyn Summariser>,
}

impl BucketSealEngine {
    pub fn new(pool: Pool, content_root: PathBuf, summariser: Arc<dyn Summariser>) -> Self {
        Self {
            tree_store: TreeStore::new(pool.clone()),
            chunk_store: ChunkStore::new(pool),
            content_store: ContentStore::new(content_root),
            summariser,
        }
    }

    /// Append a leaf to the source tree, sealing buffers as they fill.
    /// Returns the ids of any summaries that sealed during this call.
    pub async fn append_leaf(
        &self,
        owner_id: &str,
        tree: &Tree,
        chunk_id: &str,
        token_count: u32,
        timestamp_ms: i64,
    ) -> CarrierResult<Vec<String>> {
        // 1. Push leaf into L0 buffer
        self.append_to_buffer(
            owner_id,
            &tree.id,
            0,
            chunk_id,
            token_count as i64,
            timestamp_ms,
        )
        .await?;

        // 2. Cascade seals upward
        self.cascade_seals(owner_id, tree, 0, false).await
    }

    /// Append a leaf to the buffer and return whether seal should happen.
    /// Does NOT trigger the seal - the caller must enqueue a Seal job if true.
    pub async fn append_leaf_deferred(
        &self,
        owner_id: &str,
        tree: &Tree,
        chunk_id: &str,
        token_count: u32,
        timestamp_ms: i64,
    ) -> CarrierResult<bool> {
        self.append_to_buffer(
            owner_id,
            &tree.id,
            0,
            chunk_id,
            token_count as i64,
            timestamp_ms,
        )
        .await?;

        let buf = self.get_or_create_buffer(owner_id, &tree.id, 0).await?;
        Ok(should_seal(&buf))
    }

    /// Force-seal from a given level (used by time-based flush).
    pub async fn cascade_all_from(
        &self,
        owner_id: &str,
        tree: &Tree,
        start_level: u32,
        force_now: bool,
    ) -> CarrierResult<Vec<String>> {
        self.cascade_seals(owner_id, tree, start_level, force_now).await
    }

    /// Append a single item to a buffer (idempotent on item_id).
    pub(crate) async fn append_to_buffer(
        &self,
        owner_id: &str,
        tree_id: &str,
        level: u32,
        item_id: &str,
        token_delta: i64,
        item_ts_ms: i64,
    ) -> CarrierResult<()> {
        let mut buf = self.get_or_create_buffer(owner_id, tree_id, level).await?;

        // Idempotent: skip if already in buffer
        if buf.item_ids.iter().any(|existing| existing == item_id) {
            return Ok(());
        }

        buf.item_ids.push(item_id.to_string());
        buf.token_sum = buf.token_sum.saturating_add(token_delta);
        buf.oldest_at_ms = match buf.oldest_at_ms {
            Some(existing) => Some(existing.min(item_ts_ms)),
            None => Some(item_ts_ms),
        };

        self.tree_store.upsert_buffer(owner_id, &buf).await
    }

    /// Get or create a buffer for (tree_id, level).
    pub(crate) async fn get_or_create_buffer(
        &self,
        owner_id: &str,
        tree_id: &str,
        level: u32,
    ) -> CarrierResult<Buffer> {
        match self.tree_store.get_buffer(owner_id, tree_id, level).await? {
            Some(buf) => Ok(buf),
            None => Ok(Buffer {
                tree_id: tree_id.to_string(),
                level,
                item_ids: Vec::new(),
                token_sum: 0,
                oldest_at_ms: None,
            }),
        }
    }

    /// Cascade seals starting at `start_level`.
    pub(crate) async fn cascade_seals(
        &self,
        owner_id: &str,
        tree: &Tree,
        start_level: u32,
        force_now: bool,
    ) -> CarrierResult<Vec<String>> {
        let mut sealed_ids: Vec<String> = Vec::new();
        let mut level = start_level;
        let mut first_iteration = true;

        for _ in 0..MAX_CASCADE_DEPTH {
            let buf = self.get_or_create_buffer(owner_id, &tree.id, level).await?;
            let forced = first_iteration && force_now;
            first_iteration = false;

            if !forced && !should_seal(&buf) {
                break;
            }
            if buf.item_ids.is_empty() {
                break;
            }

            let summary_id = self.seal_one_level(owner_id, tree, &buf).await?;
            sealed_ids.push(summary_id);
            level += 1;
        }

        Ok(sealed_ids)
    }

    /// Seal `buf` at `level` into one summary at `level + 1`.
    async fn seal_one_level(
        &self,
        owner_id: &str,
        tree: &Tree,
        buf: &Buffer,
    ) -> CarrierResult<String> {
        let level = buf.level;
        let target_level = level + 1;

        // Hydrate inputs
        let inputs = self.hydrate_inputs(owner_id, level, &buf.item_ids).await?;
        if inputs.is_empty() {
            return Err(CarrierError::Internal(format!(
                "refused to seal empty buffer tree_id={} level={}",
                tree.id, level
            )));
        }

        // Compute envelope across children
        let time_range_start_ms = inputs.iter().map(|i| i.time_range_start_ms).min().unwrap_or(0);
        let time_range_end_ms = inputs.iter().map(|i| i.time_range_end_ms).max().unwrap_or(0);
        let score = inputs
            .iter()
            .map(|i| i.score)
            .fold(f32::NEG_INFINITY, f32::max)
            .max(0.0);

        // Run summariser (sync trait - file/LLM work) off the async runtime.
        // Summariser: Send + Sync and SummaryInput: Clone, so move owned inputs
        // + an owned tree_id into the blocking closure (SummaryContext borrows
        // &str). `inputs` is not used after this point.
        let summariser = self.summariser.clone();
        let tree_id = tree.id.clone();
        let tree_kind = tree.kind;
        let output = tokio::task::spawn_blocking(move || {
            let ctx = SummaryContext {
                tree_id: &tree_id,
                tree_kind,
                target_level,
                token_budget: OUTPUT_TOKEN_BUDGET,
            };
            summariser.summarise(&inputs, &ctx)
        })
        .await
        .map_err(|e| CarrierError::Internal(format!("summarise join: {e}")))?;

        // Build the new summary node
        let now_ms = chrono::Utc::now().timestamp_millis();
        let summary_id = format!("sum_L{}_{}", target_level, uuid::Uuid::new_v4().simple());

        let node = SummaryNode {
            id: summary_id.clone(),
            tree_id: tree.id.clone(),
            // Inherit per-user isolation from the tree being sealed.
            user_id: tree.user_id.clone(),
            tree_kind: tree.kind,
            level: target_level,
            parent_id: None,
            child_ids: buf.item_ids.clone(),
            content: output.content.clone(),
            token_count: output.token_count,
            entities: output.entities,
            topics: output.topics,
            time_range_start_ms,
            time_range_end_ms,
            score,
            sealed_at_ms: now_ms,
            deleted: false,
            embedding: None,
        };

        // Write summary content to disk (sync fs -> spawn_blocking). `node` is
        // reused below for insert_summary, so clone into the closure.
        let cs = self.content_store.clone();
        let owner = owner_id.to_string();
        let node_for_disk = node.clone();
        tokio::task::spawn_blocking(move || {
            cs.ensure_dirs(&owner)?;
            cs.write_summary(&owner, &node_for_disk)
        })
        .await
        .map_err(|e| CarrierError::Internal(format!("summary content write join: {e}")))??;

        // Persist the summary + clear this buffer + append to parent buffer.
        self.tree_store.insert_summary(owner_id, &node).await?;
        self.tree_store.clear_buffer(owner_id, &tree.id, level).await?;

        let mut parent = self
            .get_or_create_buffer(owner_id, &tree.id, target_level)
            .await?;
        parent.item_ids.push(summary_id.clone());
        parent.token_sum = parent.token_sum.saturating_add(node.token_count as i64);
        parent.oldest_at_ms = match parent.oldest_at_ms {
            Some(existing) => Some(existing.min(time_range_start_ms)),
            None => Some(time_range_start_ms),
        };
        self.tree_store.upsert_buffer(owner_id, &parent).await?;

        // Update tree max_level if needed
        if target_level > tree.max_level {
            self.tree_store
                .update_tree_after_seal(owner_id, &tree.id, target_level, now_ms)
                .await?;
        }

        tracing::info!(
            "[bucket_seal] sealed tree_id={} level={}->{} summary_id={} children={}",
            tree.id,
            level,
            target_level,
            summary_id,
            buf.item_ids.len()
        );

        Ok(summary_id)
    }

    /// Fetch contributions for `item_ids`.
    async fn hydrate_inputs(
        &self,
        owner_id: &str,
        level: u32,
        item_ids: &[String],
    ) -> CarrierResult<Vec<SummaryInput>> {
        if level == 0 {
            self.hydrate_leaf_inputs(owner_id, item_ids).await
        } else {
            self.hydrate_summary_inputs(owner_id, item_ids).await
        }
    }

    async fn hydrate_leaf_inputs(
        &self,
        owner_id: &str,
        chunk_ids: &[String],
    ) -> CarrierResult<Vec<SummaryInput>> {
        let mut out = Vec::with_capacity(chunk_ids.len());
        for id in chunk_ids {
            let chunk = match self.chunk_store.get_chunk(owner_id, None, id).await? {
                Some(c) => c,
                None => {
                    tracing::warn!("[bucket_seal] missing chunk {id} - skipping");
                    continue;
                }
            };

            // Try to read full body from disk (sync fs -> spawn_blocking), fall
            // back to DB content on any error (read error or join error).
            let cs = self.content_store.clone();
            let owner = owner_id.to_string();
            let sk = chunk.source_kind.as_str().to_string();
            let sid = chunk.source_id.clone();
            let cid = chunk.id.clone();
            let fallback = chunk.content.clone();
            let body = match tokio::task::spawn_blocking(move || {
                cs.read_chunk_body(&owner, &sk, &sid, &cid)
            })
            .await
            {
                Ok(Ok(body)) => body,
                _ => fallback,
            };

            out.push(SummaryInput {
                id: chunk.id,
                content: body,
                token_count: chunk.token_count,
                entities: Vec::new(),
                topics: Vec::new(),
                time_range_start_ms: chunk.time_range_start_ms,
                time_range_end_ms: chunk.time_range_end_ms,
                score: 0.0,
            });
        }
        Ok(out)
    }

    async fn hydrate_summary_inputs(
        &self,
        owner_id: &str,
        summary_ids: &[String],
    ) -> CarrierResult<Vec<SummaryInput>> {
        let mut out = Vec::with_capacity(summary_ids.len());
        for id in summary_ids {
            let node = match self.tree_store.get_summary(owner_id, None, id).await? {
                Some(n) => n,
                None => {
                    tracing::warn!("[bucket_seal] missing summary {id} - skipping");
                    continue;
                }
            };

            out.push(SummaryInput {
                id: node.id,
                content: node.content,
                token_count: node.token_count,
                entities: node.entities,
                topics: node.topics,
                time_range_start_ms: node.time_range_start_ms,
                time_range_end_ms: node.time_range_end_ms,
                score: node.score,
            });
        }
        Ok(out)
    }
}

/// Level-aware seal gate.
///
/// L0: seal when `token_sum >= INPUT_TOKEN_BUDGET` OR `item_ids.len() >= SUMMARY_FANOUT`.
/// L≥1: seal when `item_ids.len() >= SUMMARY_FANOUT`.
pub fn should_seal(buf: &Buffer) -> bool {
    if buf.item_ids.is_empty() {
        return false;
    }
    if buf.level == 0 {
        buf.token_sum >= INPUT_TOKEN_BUDGET as i64
            || (buf.item_ids.len() as u32) >= SUMMARY_FANOUT
    } else {
        (buf.item_ids.len() as u32) >= SUMMARY_FANOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;
    use memory::tree::summariser::inert::InertSummariser;
    use memory::tree::types::{Chunk, SourceKind};
    use tempfile::TempDir;
    use types::memory_tree::TreeKind;

    async fn setup() -> Option<(BucketSealEngine, Pool, TempDir)> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        let dir = TempDir::new().ok()?;
        let engine = BucketSealEngine::new(
            pool.clone(),
            dir.path().to_path_buf(),
            Arc::new(InertSummariser),
        );
        Some((engine, pool, dir))
    }

    async fn make_tree(pool: &Pool, owner_id: &str, scope: &str) -> Tree {
        TreeStore::new(pool.clone())
            .get_or_create_tree(owner_id, "", TreeKind::Source, scope)
            .await
            .unwrap()
    }

    async fn insert_chunk(pool: &Pool, owner_id: &str, id: &str, tokens: u32) {
        let chunk = Chunk {
            id: id.to_string(),
            owner_id: owner_id.to_string(),
            user_id: String::new(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:test:sender".to_string(),
            source_ref: None,
            timestamp_ms: 1_700_000_000_000,
            time_range_start_ms: 1_700_000_000_000,
            time_range_end_ms: 1_700_000_000_000,
            tags_json: "[]".to_string(),
            content: "test content for chunk".to_string(),
            token_count: tokens,
            seq_in_source: 0,
            partial_message: false,
            lifecycle_status: "admitted".to_string(),
            created_at_ms: 1_700_000_000_000,
        };
        ChunkStore::new(pool.clone())
            .upsert_chunks(&[chunk])
            .await
            .unwrap();
    }

    #[test]
    fn should_seal_l0_tokens() {
        let buf = Buffer {
            tree_id: "tree_1".to_string(),
            level: 0,
            item_ids: vec!["c1".to_string()],
            token_sum: INPUT_TOKEN_BUDGET as i64,
            oldest_at_ms: None,
        };
        assert!(should_seal(&buf));
    }

    #[test]
    fn should_seal_l0_count() {
        let ids: Vec<String> = (0..SUMMARY_FANOUT).map(|i| format!("c{i}")).collect();
        let buf = Buffer {
            tree_id: "tree_1".to_string(),
            level: 0,
            item_ids: ids,
            token_sum: 100,
            oldest_at_ms: None,
        };
        assert!(should_seal(&buf));
    }

    #[test]
    fn should_not_seal_l0_small() {
        let buf = Buffer {
            tree_id: "tree_1".to_string(),
            level: 0,
            item_ids: vec!["c1".to_string()],
            token_sum: 100,
            oldest_at_ms: None,
        };
        assert!(!should_seal(&buf));
    }

    #[test]
    fn should_seal_l1_count() {
        let ids: Vec<String> = (0..SUMMARY_FANOUT).map(|i| format!("s{i}")).collect();
        let buf = Buffer {
            tree_id: "tree_1".to_string(),
            level: 1,
            item_ids: ids,
            token_sum: 100,
            oldest_at_ms: None,
        };
        assert!(should_seal(&buf));
    }

    #[test]
    fn should_not_seal_l1_tokens_only() {
        let buf = Buffer {
            tree_id: "tree_1".to_string(),
            level: 1,
            item_ids: vec!["s1".to_string()],
            token_sum: INPUT_TOKEN_BUDGET as i64,
            oldest_at_ms: None,
        };
        assert!(!should_seal(&buf)); // L1+ only gates on count
    }

    #[tokio::test]
    async fn append_leaf_no_seal() {
        let (engine, pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let tree = make_tree(&pool, "owner_1", "source_1").await;
        insert_chunk(&pool, "owner_1", "chunk_1", 100).await;

        let sealed = engine
            .append_leaf("owner_1", &tree, "chunk_1", 100, 1000)
            .await
            .unwrap();
        assert!(sealed.is_empty()); // Not enough tokens to seal
    }

    #[tokio::test]
    async fn append_leaf_seals_on_budget() {
        let (engine, pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = make_tree(&pool, "owner_1", "source_1").await;

        // Add enough chunks to hit INPUT_TOKEN_BUDGET
        for i in 0..10 {
            let chunk_id = format!("chunk_{i}");
            insert_chunk(&pool, "owner_1", &chunk_id, INPUT_TOKEN_BUDGET / 10 + 1).await;
            engine
                .append_leaf(
                    "owner_1",
                    &tree,
                    &chunk_id,
                    INPUT_TOKEN_BUDGET / 10 + 1,
                    1000 + i as i64,
                )
                .await
                .unwrap();
        }

        // Verify an L1 summary was created
        let summaries = engine
            .tree_store
            .list_summaries("owner_1", None, &tree.id, Some(1), 100)
            .await
            .unwrap();
        assert!(!summaries.is_empty());
    }

    #[tokio::test]
    async fn append_leaf_idempotent() {
        let (engine, pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = make_tree(&pool, "owner_1", "source_1").await;
        insert_chunk(&pool, "owner_1", "chunk_1", 100).await;

        engine
            .append_leaf("owner_1", &tree, "chunk_1", 100, 1000)
            .await
            .unwrap();
        engine
            .append_leaf("owner_1", &tree, "chunk_1", 100, 1000)
            .await
            .unwrap();

        // Should not duplicate in buffer
        let buf = engine
            .get_or_create_buffer("owner_1", &tree.id, 0)
            .await
            .unwrap();
        assert_eq!(buf.item_ids.len(), 1);
    }

    #[tokio::test]
    async fn force_seal() {
        let (engine, pool, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = make_tree(&pool, "owner_1", "source_1").await;
        insert_chunk(&pool, "owner_1", "chunk_1", 100).await;

        // Append without reaching budget
        engine
            .append_leaf("owner_1", &tree, "chunk_1", 100, 1000)
            .await
            .unwrap();

        // Force seal
        let sealed = engine
            .cascade_all_from("owner_1", &tree, 0, true)
            .await
            .unwrap();
        assert_eq!(sealed.len(), 1);

        // Verify L1 summary created
        let summaries = engine
            .tree_store
            .list_summaries("owner_1", None, &tree.id, Some(1), 100)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
    }
}
