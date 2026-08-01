//! PG-backed chunk store - mirrors `memory::tree::store::ChunkStore`.
//!
//! `mem_tree_chunks` CRUD with per-user isolation fallback
//! `(user_id = $N OR user_id = '')`. `tags_json` is TEXT (JSON string) matching
//! `Chunk::tags_json: String`; `token_count`/`seq_in_source` are PG INT (i32)
//! cast to `Chunk`'s `u32`.

use deadpool_postgres::Pool;
use memory::tree::types::{Chunk, SourceKind, CHUNK_STATUS_ADMITTED};
use tokio_postgres::types::ToSql;
use types::error::{CarrierError, CarrierResult};

pub struct ChunkStore {
    pool: Pool,
}

impl ChunkStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    pub async fn upsert_chunks(&self, chunks: &[Chunk]) -> CarrierResult<()> {
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        for c in chunks {
            let sk = c.source_kind.as_str().to_string();
            let tc = c.token_count as i32;
            let sq = c.seq_in_source as i32;
            tx.execute(
                "INSERT INTO mem_tree_chunks \
                    (id, owner_id, user_id, agent_id, source_kind, source_id, source_ref, \
                     timestamp_ms, time_range_start_ms, time_range_end_ms, \
                     tags_json, content, token_count, seq_in_source, \
                     partial_message, lifecycle_status, created_at_ms) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17) \
                 ON CONFLICT (id) DO UPDATE SET \
                     owner_id=EXCLUDED.owner_id, user_id=EXCLUDED.user_id, \
                     agent_id=EXCLUDED.agent_id, source_kind=EXCLUDED.source_kind, \
                     source_id=EXCLUDED.source_id, source_ref=EXCLUDED.source_ref, \
                     timestamp_ms=EXCLUDED.timestamp_ms, \
                     time_range_start_ms=EXCLUDED.time_range_start_ms, \
                     time_range_end_ms=EXCLUDED.time_range_end_ms, \
                     tags_json=EXCLUDED.tags_json, content=EXCLUDED.content, \
                     token_count=EXCLUDED.token_count, seq_in_source=EXCLUDED.seq_in_source, \
                     partial_message=EXCLUDED.partial_message, \
                     lifecycle_status=EXCLUDED.lifecycle_status, \
                     created_at_ms=EXCLUDED.created_at_ms",
                &[
                    &c.id, &c.owner_id, &c.user_id, &c.agent_id, &sk, &c.source_id,
                    &c.source_ref, &c.timestamp_ms, &c.time_range_start_ms, &c.time_range_end_ms,
                    &c.tags_json, &c.content, &tc, &sq, &c.partial_message,
                    &c.lifecycle_status, &c.created_at_ms,
                ],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
        tx.commit().await.map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    pub async fn get_chunk(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        chunk_id: &str,
    ) -> CarrierResult<Option<Chunk>> {
        let client = self.client().await?;
        let row = if let Some(u) = user_id {
            client
                .query_opt(
                    "SELECT id, owner_id, user_id, agent_id, source_kind, source_id, source_ref, \
                     timestamp_ms, time_range_start_ms, time_range_end_ms, \
                     tags_json, content, token_count, seq_in_source, \
                     partial_message, lifecycle_status, created_at_ms \
                     FROM mem_tree_chunks \
                     WHERE owner_id=$1 AND id=$2 AND (user_id=$3 OR user_id='')",
                    &[&owner_id, &chunk_id, &u],
                )
                .await
        } else {
            client
                .query_opt(
                    "SELECT id, owner_id, user_id, agent_id, source_kind, source_id, source_ref, \
                     timestamp_ms, time_range_start_ms, time_range_end_ms, \
                     tags_json, content, token_count, seq_in_source, \
                     partial_message, lifecycle_status, created_at_ms \
                     FROM mem_tree_chunks WHERE owner_id=$1 AND id=$2",
                    &[&owner_id, &chunk_id],
                )
                .await
        }
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        row.map(|r| Self::row_to_chunk(&r)).transpose()
    }

    pub async fn list_chunks(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        source_kind: Option<&SourceKind>,
        source_id: Option<&str>,
        lifecycle_status: Option<&str>,
        limit: usize,
    ) -> CarrierResult<Vec<Chunk>> {
        let client = self.client().await?;
        // Own the filter values so params can borrow them with a single lifetime
        // (&str bindings inside `if let` don't live long enough for the params vec).
        let owner = owner_id.to_string();
        let uid = user_id.map(str::to_string);
        let sk = source_kind.map(|s| s.as_str().to_string());
        let sid = source_id.map(str::to_string);
        let ls = lifecycle_status.map(str::to_string);
        let lim = limit as i64;

        let mut sql = "SELECT id, owner_id, user_id, agent_id, source_kind, source_id, source_ref, \
                       timestamp_ms, time_range_start_ms, time_range_end_ms, \
                       tags_json, content, token_count, seq_in_source, \
                       partial_message, lifecycle_status, created_at_ms \
                       FROM mem_tree_chunks WHERE owner_id=$1"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner];
        let mut i = 2;
        if let Some(u) = &uid {
            sql.push_str(&format!(" AND (user_id=${i} OR user_id='')"));
            params.push(u);
            i += 1;
        }
        if let Some(s) = &sk {
            sql.push_str(&format!(" AND source_kind=${i}"));
            params.push(s);
            i += 1;
        }
        if let Some(s) = &sid {
            sql.push_str(&format!(" AND source_id=${i}"));
            params.push(s);
            i += 1;
        }
        if let Some(s) = &ls {
            sql.push_str(&format!(" AND lifecycle_status=${i}"));
            params.push(s);
            i += 1;
        }
        sql.push_str(&format!(" ORDER BY timestamp_ms ASC LIMIT ${i}"));
        params.push(&lim);

        let rows = client
            .query(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter().map(Self::row_to_chunk).collect()
    }

    pub async fn update_lifecycle(
        &self,
        owner_id: &str,
        chunk_id: &str,
        new_status: &str,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        client
            .execute(
                "UPDATE mem_tree_chunks SET lifecycle_status=$1 WHERE owner_id=$2 AND id=$3",
                &[&new_status, &owner_id, &chunk_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    pub async fn mark_admitted(&self, owner_id: &str, chunk_ids: &[String]) -> CarrierResult<()> {
        if chunk_ids.is_empty() {
            return Ok(());
        }
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        for cid in chunk_ids {
            tx.execute(
                "UPDATE mem_tree_chunks SET lifecycle_status=$1 WHERE owner_id=$2 AND id=$3",
                &[&CHUNK_STATUS_ADMITTED, &owner_id, cid],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
        tx.commit().await.map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    pub async fn count_chunks(
        &self,
        owner_id: &str,
        lifecycle_status: Option<&str>,
    ) -> CarrierResult<usize> {
        let client = self.client().await?;
        let count: i64 = match lifecycle_status {
            Some(ls) => {
                let row = client
                    .query_one(
                        "SELECT COUNT(*) FROM mem_tree_chunks WHERE owner_id=$1 AND lifecycle_status=$2",
                        &[&owner_id, &ls],
                    )
                    .await
                    .map_err(|e| CarrierError::Memory(e.to_string()))?;
                row.get(0)
            }
            None => {
                let row = client
                    .query_one(
                        "SELECT COUNT(*) FROM mem_tree_chunks WHERE owner_id=$1",
                        &[&owner_id],
                    )
                    .await
                    .map_err(|e| CarrierError::Memory(e.to_string()))?;
                row.get(0)
            }
        };
        Ok(count as usize)
    }

    async fn client(&self) -> CarrierResult<deadpool_postgres::Object> {
        self.pool
            .get()
            .await
            .map_err(|e| CarrierError::Internal(format!("pg pool get: {e}")))
    }

    fn row_to_chunk(row: &tokio_postgres::Row) -> CarrierResult<Chunk> {
        let source_kind_str: String = row
            .try_get(4)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let source_kind = match source_kind_str.as_str() {
            "chat" => SourceKind::Chat,
            "email" => SourceKind::Email,
            "document" => SourceKind::Document,
            _ => SourceKind::Chat,
        };
        Ok(Chunk {
            id: row.try_get(0).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            owner_id: row.try_get(1).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            user_id: row.try_get(2).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            agent_id: row.try_get(3).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            source_kind,
            source_id: row.try_get(5).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            source_ref: row.try_get(6).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            timestamp_ms: row.try_get(7).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            time_range_start_ms: row.try_get(8).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            time_range_end_ms: row.try_get(9).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            tags_json: row.try_get(10).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            content: row.try_get(11).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            token_count: row
                .try_get::<_, i32>(12)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            seq_in_source: row
                .try_get::<_, i32>(13)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            partial_message: row.try_get(14).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            lifecycle_status: row.try_get(15).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            created_at_ms: row.try_get(16).map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;

    async fn setup() -> Option<ChunkStore> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        Some(ChunkStore::new(pool))
    }

    fn make_chunk(owner: &str, id_suffix: &str, seq: u32) -> Chunk {
        Chunk {
            id: format!("chunk_{id_suffix}"),
            owner_id: owner.to_string(),
            user_id: String::new(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:gh_abc:sender_1".to_string(),
            source_ref: None,
            timestamp_ms: 1000 + seq as i64 * 1000,
            time_range_start_ms: 1000 + seq as i64 * 1000,
            time_range_end_ms: 2000 + seq as i64 * 1000,
            tags_json: "[]".to_string(),
            content: format!("Hello {id_suffix}"),
            token_count: 5,
            seq_in_source: seq,
            partial_message: false,
            lifecycle_status: "admitted".to_string(),
            created_at_ms: 1000,
        }
    }

    #[tokio::test]
    async fn upsert_and_get() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let chunk = make_chunk("owner_1", "001", 0);
        store.upsert_chunks(std::slice::from_ref(&chunk)).await.unwrap();
        let got = store.get_chunk("owner_1", None, "chunk_001").await.unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().content, "Hello 001");
    }

    #[tokio::test]
    async fn get_missing() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        assert!(store.get_chunk("owner_1", None, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn owner_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let chunk = make_chunk("owner_1", "001", 0);
        store.upsert_chunks(&[chunk]).await.unwrap();
        assert!(store.get_chunk("owner_2", None, "chunk_001").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_chunk_user_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let mut alice = make_chunk("owner_1", "alice", 0);
        alice.user_id = "alice".to_string();
        store.upsert_chunks(&[alice]).await.unwrap();
        assert!(store.get_chunk("owner_1", Some("bob"), "chunk_alice").await.unwrap().is_none());
        assert!(store.get_chunk("owner_1", Some("alice"), "chunk_alice").await.unwrap().is_some());

        let shared = make_chunk("owner_1", "shared", 0);
        store.upsert_chunks(&[shared]).await.unwrap();
        assert!(store.get_chunk("owner_1", Some("alice"), "chunk_shared").await.unwrap().is_some());
        assert!(store.get_chunk("owner_1", Some("bob"), "chunk_shared").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_chunks_filter() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let c1 = make_chunk("owner_1", "001", 0);
        let c2 = make_chunk("owner_1", "002", 1);
        store.upsert_chunks(&[c1, c2]).await.unwrap();
        let all = store
            .list_chunks("owner_1", None, None, None, None, 100)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        let filtered = store
            .list_chunks("owner_1", None, Some(&SourceKind::Chat), None, None, 100)
            .await
            .unwrap();
        assert_eq!(filtered.len(), 2);
    }

    #[tokio::test]
    async fn update_lifecycle() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let mut chunk = make_chunk("owner_1", "001", 0);
        chunk.lifecycle_status = "pending_extraction".to_string();
        store.upsert_chunks(&[chunk]).await.unwrap();
        store
            .update_lifecycle("owner_1", "chunk_001", "admitted")
            .await
            .unwrap();
        let got = store.get_chunk("owner_1", None, "chunk_001").await.unwrap().unwrap();
        assert_eq!(got.lifecycle_status, "admitted");
    }

    #[tokio::test]
    async fn count_chunks() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store
            .upsert_chunks(&[make_chunk("owner_1", "001", 0), make_chunk("owner_1", "002", 1)])
            .await
            .unwrap();
        assert_eq!(store.count_chunks("owner_1", None).await.unwrap(), 2);
        assert_eq!(store.count_chunks("owner_1", Some("admitted")).await.unwrap(), 2);
        assert_eq!(store.count_chunks("owner_1", Some("pending_extraction")).await.unwrap(), 0);
    }
}
