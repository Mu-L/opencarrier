//! PG-backed tree + summary + buffer store - mirrors `memory::tree::tree_store::TreeTreeStore`.
//!
//! Kept as one struct (trees+summaries+buffers together) to match the source
//! 1:1, so the later jobs/bucket_seal port is a straightforward find-replace of
//! `TreeTreeStore` -> `TreeStore` (rather than refactoring every call site to
//! use two structs). PG type notes: `max_level`/`level`/`token_count` are INT
//! (i32) cast to the structs' `u32`; `score` is DOUBLE PRECISION (f64) cast to
//! `SummaryNode.score: f32`; `token_sum` INT (i32) cast to `Buffer.token_sum:
//! i64`; `deleted` is BOOLEAN (read directly as bool, no i32 cast); `embedding`
//! is BYTEA holding `serde_json`-serialized `Vec<f32>` bytes (matches the
//! SQLite BLOB round-trip). Per-user isolation uses the `(user_id=$N OR
//! user_id='')` fallback clause throughout.

use deadpool_postgres::Pool;
use memory::tree::types::{Buffer, SummaryNode, Tree, TreeStatus};
use tokio_postgres::types::ToSql;
use types::error::{CarrierError, CarrierResult};
use types::memory_tree::{TreeKind, TreeSummary};

/// Tree + summary + buffer store backed by PG.
#[derive(Clone)]
pub struct TreeStore {
    pool: Pool,
}

impl TreeStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    // -- Tree operations ---------------------------------------------------

    /// Get or create a tree for (owner_id, kind, scope). Returns the tree.
    ///
    /// `user_id` is persisted on creation (source trees carry the sender;
    /// topic/global trees pass `""` for owner-shared). The lookup itself keys on
    /// (owner_id, kind, scope) - source-tree scopes already encode the sender.
    ///
    /// Race-free: `INSERT ... ON CONFLICT DO NOTHING` then `SELECT` the
    /// canonical row on the same connection - two concurrent callers can't both
    /// win the insert (the unique `(owner_id, kind, scope)` index arbitrates),
    /// and both return the same row.
    pub async fn get_or_create_tree(
        &self,
        owner_id: &str,
        user_id: &str,
        kind: TreeKind,
        scope: &str,
    ) -> CarrierResult<Tree> {
        let client = self.client().await?;
        let kind_str = kind.as_str().to_string();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let id = format!("tree_{}", uuid::Uuid::new_v4().simple());

        client
            .execute(
                "INSERT INTO mem_tree_trees \
                    (id, owner_id, user_id, kind, scope, root_id, max_level, status, \
                     created_at_ms, last_sealed_at_ms) \
                 VALUES ($1,$2,$3,$4,$5,NULL,0,'active',$6,NULL) \
                 ON CONFLICT (owner_id, kind, scope) DO NOTHING",
                &[&id, &owner_id, &user_id, &kind_str, &scope, &now_ms],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        // Whether we inserted or it pre-existed, the canonical row is now
        // visible on this connection - fetch it by the unique key.
        let row = client
            .query_one(
                "SELECT id, owner_id, kind, scope, root_id, max_level, status, \
                        created_at_ms, last_sealed_at_ms, user_id \
                 FROM mem_tree_trees WHERE owner_id=$1 AND kind=$2 AND scope=$3",
                &[&owner_id, &kind_str, &scope],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Self::row_to_tree(&row)
    }

    /// Get a tree by ID.
    ///
    /// When `user_id` is `Some(u)`, only return the tree if it belongs to `u`
    /// or is owner-shared (`user_id = ''`). `None` skips the filter.
    pub async fn get_tree(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        tree_id: &str,
    ) -> CarrierResult<Option<Tree>> {
        let client = self.client().await?;
        let owner = owner_id.to_string();
        let tid = tree_id.to_string();
        let uid = user_id.map(str::to_string);

        let mut sql = "SELECT id, owner_id, kind, scope, root_id, max_level, status, \
                       created_at_ms, last_sealed_at_ms, user_id \
                 FROM mem_tree_trees WHERE owner_id=$1 AND id=$2"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner, &tid];
        if let Some(u) = &uid {
            sql.push_str(" AND (user_id=$3 OR user_id='')");
            params.push(u);
        }
        let row = client
            .query_opt(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        row.map(|r| Self::row_to_tree(&r)).transpose()
    }

    /// List all trees for an owner, optionally filtered by user and kind.
    pub async fn list_trees(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        kind: Option<TreeKind>,
        limit: usize,
    ) -> CarrierResult<Vec<TreeSummary>> {
        let client = self.client().await?;
        let owner = owner_id.to_string();
        let uid = user_id.map(str::to_string);
        let k = kind.map(|k| k.as_str().to_string());
        let lim = limit as i64;

        // $1 (owner) is referenced twice: in the summaries-count subquery and
        // the outer WHERE. PG allows reusing a positional param, so the params
        // vec still has one entry per distinct value.
        let mut sql = "SELECT t.id, t.kind, t.scope, t.status, t.max_level, \
                              0::bigint as chunk_count, \
                              COALESCE(s.cnt, 0)::bigint as summary_count, \
                              t.last_sealed_at_ms \
                       FROM mem_tree_trees t \
                       LEFT JOIN (SELECT tree_id, COUNT(*) as cnt \
                                  FROM mem_tree_summaries \
                                  WHERE owner_id=$1 AND deleted=false \
                                  GROUP BY tree_id) s ON s.tree_id = t.id \
                       WHERE t.owner_id=$1"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner];
        let mut i = 2;
        if let Some(u) = &uid {
            sql.push_str(&format!(" AND (t.user_id=${i} OR t.user_id='')"));
            params.push(u);
            i += 1;
        }
        if let Some(kv) = &k {
            sql.push_str(&format!(" AND t.kind=${i}"));
            params.push(kv);
            i += 1;
        }
        sql.push_str(&format!(" ORDER BY t.created_at_ms DESC LIMIT ${i}"));
        params.push(&lim);

        let rows = client
            .query(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter().map(Self::row_to_tree_summary).collect()
    }

    /// Update tree max_level, root_id and last_sealed_at_ms after a seal.
    pub async fn update_tree_after_seal(
        &self,
        owner_id: &str,
        tree_id: &str,
        new_max_level: u32,
        sealed_at_ms: i64,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let lvl = new_max_level as i32;
        client
            .execute(
                "UPDATE mem_tree_trees SET max_level=$1, last_sealed_at_ms=$2, \
                 root_id = COALESCE(root_id, (
                    SELECT id FROM mem_tree_summaries
                    WHERE owner_id=$3 AND tree_id=$4 AND deleted=false
                    ORDER BY level DESC, sealed_at_ms DESC LIMIT 1
                 )) \
                 WHERE owner_id=$3 AND id=$4",
                &[&lvl, &sealed_at_ms, &owner_id, &tree_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    // -- Summary operations ------------------------------------------------

    /// Insert (or replace) a summary node.
    pub async fn insert_summary(&self, owner_id: &str, summary: &SummaryNode) -> CarrierResult<()> {
        let client = self.client().await?;
        let tree_kind = summary.tree_kind.as_str().to_string();
        let level = summary.level as i32;
        let token_count = summary.token_count as i32;
        let score = summary.score as f64;
        let child_ids_json = serde_json::to_string(&summary.child_ids)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let entities_json = serde_json::to_string(&summary.entities)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let topics_json = serde_json::to_string(&summary.topics)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        // embedding: serialize Vec<f32> as JSON bytes into BYTEA (matches the
        // SQLite BLOB round-trip so migration is byte-compatible).
        let embedding: Option<Vec<u8>> = summary
            .embedding
            .as_ref()
            .map(|e| serde_json::to_vec(e).unwrap_or_default());

        client
            .execute(
                "INSERT INTO mem_tree_summaries \
                    (id, owner_id, user_id, tree_id, tree_kind, level, parent_id, \
                     child_ids_json, content, token_count, entities_json, topics_json, \
                     time_range_start_ms, time_range_end_ms, score, sealed_at_ms, \
                     deleted, embedding) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18) \
                 ON CONFLICT (id) DO UPDATE SET \
                     owner_id=EXCLUDED.owner_id, user_id=EXCLUDED.user_id, \
                     tree_id=EXCLUDED.tree_id, tree_kind=EXCLUDED.tree_kind, \
                     level=EXCLUDED.level, parent_id=EXCLUDED.parent_id, \
                     child_ids_json=EXCLUDED.child_ids_json, content=EXCLUDED.content, \
                     token_count=EXCLUDED.token_count, entities_json=EXCLUDED.entities_json, \
                     topics_json=EXCLUDED.topics_json, \
                     time_range_start_ms=EXCLUDED.time_range_start_ms, \
                     time_range_end_ms=EXCLUDED.time_range_end_ms, score=EXCLUDED.score, \
                     sealed_at_ms=EXCLUDED.sealed_at_ms, deleted=EXCLUDED.deleted, \
                     embedding=EXCLUDED.embedding",
                &[
                    &summary.id,
                    &owner_id,
                    &summary.user_id,
                    &summary.tree_id,
                    &tree_kind,
                    &level,
                    &summary.parent_id,
                    &child_ids_json,
                    &summary.content,
                    &token_count,
                    &entities_json,
                    &topics_json,
                    &summary.time_range_start_ms,
                    &summary.time_range_end_ms,
                    &score,
                    &summary.sealed_at_ms,
                    &summary.deleted,
                    &embedding,
                ],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Get a summary node by ID.
    ///
    /// When `user_id` is `Some(u)`, enforce per-user isolation (own summary or
    /// owner-shared). `None` skips the filter (write-path lookups by exact id).
    pub async fn get_summary(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        summary_id: &str,
    ) -> CarrierResult<Option<SummaryNode>> {
        let client = self.client().await?;
        let owner = owner_id.to_string();
        let sid = summary_id.to_string();
        let uid = user_id.map(str::to_string);

        let mut sql = "SELECT id, tree_id, tree_kind, level, parent_id, child_ids_json, \
                       content, token_count, entities_json, topics_json, \
                       time_range_start_ms, time_range_end_ms, score, sealed_at_ms, \
                       deleted, embedding, user_id \
                 FROM mem_tree_summaries WHERE owner_id=$1 AND id=$2"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner, &sid];
        if let Some(u) = &uid {
            sql.push_str(" AND (user_id=$3 OR user_id='')");
            params.push(u);
        }
        let row = client
            .query_opt(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        row.map(|r| Self::row_to_summary(&r)).transpose()
    }

    /// List summary nodes for a tree at a given level.
    ///
    /// When `user_id` is `Some(u)`, only return summaries belonging to `u` or
    /// owner-shared. `None` skips the filter.
    pub async fn list_summaries(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        tree_id: &str,
        level: Option<u32>,
        limit: usize,
    ) -> CarrierResult<Vec<SummaryNode>> {
        let client = self.client().await?;
        let owner = owner_id.to_string();
        let tid = tree_id.to_string();
        let uid = user_id.map(str::to_string);
        let lvl = level.map(|l| l as i32);
        let lim = limit as i64;

        let mut sql = "SELECT id, tree_id, tree_kind, level, parent_id, child_ids_json, \
                       content, token_count, entities_json, topics_json, \
                       time_range_start_ms, time_range_end_ms, score, sealed_at_ms, \
                       deleted, embedding, user_id \
                       FROM mem_tree_summaries \
                       WHERE owner_id=$1 AND tree_id=$2 AND deleted=false"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner, &tid];
        let mut i = 3;
        if let Some(u) = &uid {
            sql.push_str(&format!(" AND (user_id=${i} OR user_id='')"));
            params.push(u);
            i += 1;
        }
        if let Some(l) = &lvl {
            sql.push_str(&format!(" AND level=${i}"));
            params.push(l);
            i += 1;
        }
        sql.push_str(&format!(" ORDER BY sealed_at_ms ASC LIMIT ${i}"));
        params.push(&lim);

        let rows = client
            .query(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter().map(Self::row_to_summary).collect()
    }

    /// Soft-delete a summary node.
    pub async fn delete_summary(&self, owner_id: &str, summary_id: &str) -> CarrierResult<()> {
        let client = self.client().await?;
        client
            .execute(
                "UPDATE mem_tree_summaries SET deleted=true WHERE owner_id=$1 AND id=$2",
                &[&owner_id, &summary_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    // -- Buffer operations -------------------------------------------------

    /// Get the buffer for a tree at a given level.
    pub async fn get_buffer(
        &self,
        owner_id: &str,
        tree_id: &str,
        level: u32,
    ) -> CarrierResult<Option<Buffer>> {
        let client = self.client().await?;
        let lvl = level as i32;
        let row = client
            .query_opt(
                "SELECT tree_id, level, item_ids_json, token_sum, oldest_at_ms \
                 FROM mem_tree_buffers WHERE owner_id=$1 AND tree_id=$2 AND level=$3",
                &[&owner_id, &tree_id, &lvl],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        row.map(|r| Self::row_to_buffer(&r)).transpose()
    }

    /// Upsert a buffer (insert or replace).
    pub async fn upsert_buffer(&self, owner_id: &str, buffer: &Buffer) -> CarrierResult<()> {
        let client = self.client().await?;
        let level = buffer.level as i32;
        let item_ids_json = serde_json::to_string(&buffer.item_ids)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let token_sum = buffer.token_sum as i32;
        let now_ms = chrono::Utc::now().timestamp_millis();

        client
            .execute(
                "INSERT INTO mem_tree_buffers \
                    (tree_id, level, owner_id, item_ids_json, token_sum, oldest_at_ms, \
                     updated_at_ms) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7) \
                 ON CONFLICT (tree_id, level) DO UPDATE SET \
                     owner_id=EXCLUDED.owner_id, item_ids_json=EXCLUDED.item_ids_json, \
                     token_sum=EXCLUDED.token_sum, oldest_at_ms=EXCLUDED.oldest_at_ms, \
                     updated_at_ms=EXCLUDED.updated_at_ms",
                &[
                    &buffer.tree_id,
                    &level,
                    &owner_id,
                    &item_ids_json,
                    &token_sum,
                    &buffer.oldest_at_ms,
                    &now_ms,
                ],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Clear a buffer (remove all items) after a seal.
    pub async fn clear_buffer(
        &self,
        owner_id: &str,
        tree_id: &str,
        level: u32,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let lvl = level as i32;
        let now_ms = chrono::Utc::now().timestamp_millis();
        client
            .execute(
                "UPDATE mem_tree_buffers SET item_ids_json='[]', token_sum=0, \
                 oldest_at_ms=NULL, updated_at_ms=$1 \
                 WHERE owner_id=$2 AND tree_id=$3 AND level=$4",
                &[&now_ms, &owner_id, &tree_id, &lvl],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// List buffers with items older than the cutoff timestamp.
    pub async fn list_stale_buffers(
        &self,
        owner_id: &str,
        cutoff_ms: i64,
    ) -> CarrierResult<Vec<Buffer>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT tree_id, level, item_ids_json, token_sum, oldest_at_ms \
                 FROM mem_tree_buffers \
                 WHERE owner_id=$1 \
                   AND oldest_at_ms IS NOT NULL \
                   AND oldest_at_ms <= $2 \
                   AND item_ids_json != '[]'",
                &[&owner_id, &cutoff_ms],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter().map(Self::row_to_buffer).collect()
    }

    // -- Helpers -----------------------------------------------------------

    async fn client(&self) -> CarrierResult<deadpool_postgres::Object> {
        self.pool
            .get()
            .await
            .map_err(|e| CarrierError::Internal(format!("pg pool get: {e}")))
    }

    fn parse_kind(s: &str) -> TreeKind {
        match s {
            "source" => TreeKind::Source,
            "topic" => TreeKind::Topic,
            "global" => TreeKind::Global,
            _ => TreeKind::Source,
        }
    }

    fn parse_status(s: &str) -> TreeStatus {
        match s {
            "archived" => TreeStatus::Archived,
            _ => TreeStatus::Active,
        }
    }

    fn row_to_tree(row: &tokio_postgres::Row) -> CarrierResult<Tree> {
        let kind_str: String = row
            .try_get(2)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let status_str: String = row
            .try_get(6)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        Ok(Tree {
            id: row
                .try_get(0)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            owner_id: row
                .try_get(1)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            kind: Self::parse_kind(&kind_str),
            scope: row
                .try_get(3)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            root_id: row
                .try_get(4)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            max_level: row
                .try_get::<_, i32>(5)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?
                as u32,
            status: Self::parse_status(&status_str),
            created_at_ms: row
                .try_get(7)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            last_sealed_at_ms: row
                .try_get(8)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            user_id: row
                .try_get(9)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }

    fn row_to_summary(row: &tokio_postgres::Row) -> CarrierResult<SummaryNode> {
        let tree_kind_str: String = row
            .try_get(2)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let child_ids_json: String = row
            .try_get(5)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let entities_json: String = row
            .try_get(8)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let topics_json: String = row
            .try_get(9)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let embedding_blob: Option<Vec<u8>> = row
            .try_get(15)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let embedding = embedding_blob.and_then(|b| serde_json::from_slice::<Vec<f32>>(&b).ok());

        Ok(SummaryNode {
            id: row
                .try_get(0)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            tree_id: row
                .try_get(1)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            tree_kind: Self::parse_kind(&tree_kind_str),
            level: row
                .try_get::<_, i32>(3)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            parent_id: row
                .try_get(4)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            child_ids: serde_json::from_str(&child_ids_json).unwrap_or_default(),
            content: row
                .try_get(6)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            token_count: row
                .try_get::<_, i32>(7)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?
                as u32,
            entities: serde_json::from_str(&entities_json).unwrap_or_default(),
            topics: serde_json::from_str(&topics_json).unwrap_or_default(),
            time_range_start_ms: row
                .try_get(10)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            time_range_end_ms: row
                .try_get(11)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            score: row
                .try_get::<_, f64>(12)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as f32,
            sealed_at_ms: row
                .try_get(13)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            deleted: row
                .try_get(14)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            embedding,
            user_id: row
                .try_get(16)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }

    fn row_to_tree_summary(row: &tokio_postgres::Row) -> CarrierResult<TreeSummary> {
        Ok(TreeSummary {
            tree_id: row
                .try_get(0)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            kind: row
                .try_get(1)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            scope: row
                .try_get(2)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            status: row
                .try_get(3)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            max_level: row
                .try_get::<_, i32>(4)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?
                as u32,
            chunk_count: row
                .try_get::<_, i64>(5)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?
                as usize,
            summary_count: row
                .try_get::<_, i64>(6)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?
                as usize,
            last_sealed_at_ms: row
                .try_get(7)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }

    fn row_to_buffer(row: &tokio_postgres::Row) -> CarrierResult<Buffer> {
        let item_ids_json: String = row
            .try_get(2)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        Ok(Buffer {
            tree_id: row
                .try_get(0)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            level: row
                .try_get::<_, i32>(1)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            item_ids: serde_json::from_str(&item_ids_json).unwrap_or_default(),
            token_sum: row
                .try_get::<_, i32>(3)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?
                as i64,
            oldest_at_ms: row
                .try_get(4)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;

    async fn setup() -> Option<TreeStore> {
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
        Some(TreeStore::new(pool))
    }

    fn make_summary(tree_id: &str, id: &str, level: u32, parent: Option<&str>) -> SummaryNode {
        SummaryNode {
            id: id.to_string(),
            tree_id: tree_id.to_string(),
            user_id: String::new(),
            tree_kind: TreeKind::Source,
            level,
            parent_id: parent.map(str::to_string),
            child_ids: vec!["chunk_1".to_string()],
            content: format!("Summary {id}"),
            token_count: 50,
            entities: vec!["person:Alice".to_string()],
            topics: vec!["project-phoenix".to_string()],
            time_range_start_ms: 1000,
            time_range_end_ms: 5000,
            score: 0.85,
            sealed_at_ms: 6000 + level as i64 * 1000,
            deleted: false,
            embedding: None,
        }
    }

    #[tokio::test]
    async fn get_or_create_tree() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:gh_abc:sender_1")
            .await
            .unwrap();
        assert_eq!(tree.owner_id, "owner_1");
        assert_eq!(tree.kind, TreeKind::Source);
        assert_eq!(tree.scope, "wechat:gh_abc:sender_1");
        assert_eq!(tree.status, TreeStatus::Active);

        // Same call returns the same tree (idempotent).
        let tree2 = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:gh_abc:sender_1")
            .await
            .unwrap();
        assert_eq!(tree.id, tree2.id);
    }

    #[tokio::test]
    async fn list_trees_owner_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_2")
            .await
            .unwrap();
        store
            .get_or_create_tree("owner_1", "", TreeKind::Global, "global")
            .await
            .unwrap();

        let all = store.list_trees("owner_1", None, None, 100).await.unwrap();
        assert_eq!(all.len(), 3);

        let sources = store
            .list_trees("owner_1", None, Some(TreeKind::Source), 100)
            .await
            .unwrap();
        assert_eq!(sources.len(), 2);

        // Different owner sees nothing.
        let empty = store.list_trees("owner_2", None, None, 100).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn get_tree_user_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        // Alice's source tree.
        let alice_tree = store
            .get_or_create_tree("owner_1", "alice", TreeKind::Source, "src:alice")
            .await
            .unwrap();
        // Owner-shared global tree.
        let shared = store
            .get_or_create_tree("owner_1", "", TreeKind::Global, "global")
            .await
            .unwrap();

        // Bob can't see Alice's tree but can see the owner-shared global.
        assert!(store
            .get_tree("owner_1", Some("bob"), &alice_tree.id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_tree("owner_1", Some("alice"), &alice_tree.id)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_tree("owner_1", Some("bob"), &shared.id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn insert_and_get_summary() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        let summary = make_summary(&tree.id, "sum_001", 1, None);
        store.insert_summary("owner_1", &summary).await.unwrap();

        let got = store
            .get_summary("owner_1", None, "sum_001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.content, "Summary sum_001");
        assert_eq!(got.entities.len(), 1);
        assert_eq!(got.tree_kind, TreeKind::Source);
        assert!((got.score - 0.85f32).abs() < 1e-6);
    }

    #[tokio::test]
    async fn get_summary_user_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        let mut alice_sum = make_summary(&tree.id, "sum_alice", 1, None);
        alice_sum.user_id = "alice".to_string();
        store.insert_summary("owner_1", &alice_sum).await.unwrap();

        // Bob can't see Alice's summary; Alice can; None (write-path) can.
        assert!(store
            .get_summary("owner_1", Some("bob"), "sum_alice")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_summary("owner_1", Some("alice"), "sum_alice")
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_summary("owner_1", None, "sum_alice")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn list_summaries_level_filter() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        store
            .insert_summary("owner_1", &make_summary(&tree.id, "sum_l1", 1, None))
            .await
            .unwrap();
        store
            .insert_summary(
                "owner_1",
                &make_summary(&tree.id, "sum_l2a", 2, Some("sum_l1")),
            )
            .await
            .unwrap();
        store
            .insert_summary(
                "owner_1",
                &make_summary(&tree.id, "sum_l2b", 2, Some("sum_l1")),
            )
            .await
            .unwrap();

        let all = store
            .list_summaries("owner_1", None, &tree.id, None, 100)
            .await
            .unwrap();
        assert_eq!(all.len(), 3);

        let l2 = store
            .list_summaries("owner_1", None, &tree.id, Some(2), 100)
            .await
            .unwrap();
        assert_eq!(l2.len(), 2);
        assert!(l2.iter().all(|s| s.level == 2));
    }

    #[tokio::test]
    async fn delete_summary() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        store
            .insert_summary("owner_1", &make_summary(&tree.id, "sum_del", 1, None))
            .await
            .unwrap();
        store.delete_summary("owner_1", "sum_del").await.unwrap();

        // list_summaries filters deleted=false, so it disappears.
        let listed = store
            .list_summaries("owner_1", None, &tree.id, None, 100)
            .await
            .unwrap();
        assert!(listed.is_empty());
        // But get_summary (write-path, no deleted filter) still finds it.
        let got = store
            .get_summary("owner_1", None, "sum_del")
            .await
            .unwrap()
            .unwrap();
        assert!(got.deleted);
    }

    #[tokio::test]
    async fn buffer_upsert_and_get() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        let buf = Buffer {
            tree_id: tree.id.clone(),
            level: 0,
            item_ids: vec!["chunk_1".to_string(), "chunk_2".to_string()],
            token_sum: 1500,
            oldest_at_ms: Some(1000),
        };
        store.upsert_buffer("owner_1", &buf).await.unwrap();

        let got = store
            .get_buffer("owner_1", &tree.id, 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.item_ids.len(), 2);
        assert_eq!(got.token_sum, 1500);
        assert_eq!(got.oldest_at_ms, Some(1000));
    }

    #[tokio::test]
    async fn buffer_clear() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        let buf = Buffer {
            tree_id: tree.id.clone(),
            level: 0,
            item_ids: vec!["chunk_1".to_string()],
            token_sum: 500,
            oldest_at_ms: Some(1000),
        };
        store.upsert_buffer("owner_1", &buf).await.unwrap();
        store.clear_buffer("owner_1", &tree.id, 0).await.unwrap();

        let got = store
            .get_buffer("owner_1", &tree.id, 0)
            .await
            .unwrap()
            .unwrap();
        assert!(got.item_ids.is_empty());
        assert_eq!(got.token_sum, 0);
        assert_eq!(got.oldest_at_ms, None);
    }

    #[tokio::test]
    async fn list_stale_buffers() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        // Old, non-empty buffer -> stale.
        store
            .upsert_buffer(
                "owner_1",
                &Buffer {
                    tree_id: tree.id.clone(),
                    level: 0,
                    item_ids: vec!["c1".to_string()],
                    token_sum: 100,
                    oldest_at_ms: Some(500),
                },
            )
            .await
            .unwrap();
        // Newer buffer -> not stale (above cutoff).
        store
            .upsert_buffer(
                "owner_1",
                &Buffer {
                    tree_id: tree.id.clone(),
                    level: 1,
                    item_ids: vec!["c2".to_string()],
                    token_sum: 100,
                    oldest_at_ms: Some(5000),
                },
            )
            .await
            .unwrap();

        let stale = store.list_stale_buffers("owner_1", 1000).await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].level, 0);
    }

    #[tokio::test]
    async fn update_tree_after_seal_sets_root() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree = store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "source_1")
            .await
            .unwrap();
        // Before any seal: no root, max_level 0.
        let before = store
            .get_tree("owner_1", None, &tree.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.root_id, None);
        assert_eq!(before.max_level, 0);

        // Insert a level-1 summary, then seal.
        store
            .insert_summary("owner_1", &make_summary(&tree.id, "sum_root", 1, None))
            .await
            .unwrap();
        store
            .update_tree_after_seal("owner_1", &tree.id, 1, 9000)
            .await
            .unwrap();

        let after = store
            .get_tree("owner_1", None, &tree.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.max_level, 1);
        assert_eq!(after.last_sealed_at_ms, Some(9000));
        // root_id backfilled to the highest-level summary.
        assert_eq!(after.root_id, Some("sum_root".to_string()));
    }
}
