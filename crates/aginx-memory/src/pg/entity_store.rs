//! PG-backed entity index + hotness store - mirrors `memory::tree::entity_store::EntityStore`.
//!
//! Owns `mem_tree_entity_index` (per-user, with the `(user_id=$N OR user_id='')`
//! fallback) and `mem_tree_entity_hotness` (owner-level aggregate, no user_id).
//! `f32` score/hotness/graph_centrality values bind to and read from DOUBLE
//! PRECISION as `f64`. `bump_entity_hotness` uses an atomic `ON CONFLICT DO
//! UPDATE` upsert instead of the SQLite store's update-then-insert (which races
//! under a multi-connection pool - two callers could both see 0 rows updated and
//! both insert, hitting the PK). Reuses `EntityIndexEntry` / `HotnessCounters`
//! / `EntityMatch` from the memory crate to avoid type drift.

use deadpool_postgres::Pool;
use memory::tree::entity_store::EntityIndexEntry;
use memory::tree::types::{EntityKind, HotnessCounters};
use tokio_postgres::types::ToSql;
use types::error::{CarrierError, CarrierResult};
use types::memory_tree::EntityMatch;

/// Entity index + hotness store backed by PG.
pub struct EntityStore {
    pool: Pool,
}

impl EntityStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    // -- Entity index ------------------------------------------------------

    /// Upsert an entity index entry (insert or replace by (owner, entity, node) PK).
    pub async fn upsert_entity_index(
        &self,
        owner_id: &str,
        entry: &EntityIndexEntry<'_>,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let entity_kind = entry.entity_kind.as_str().to_string();
        let score = entry.score as f64;

        client
            .execute(
                "INSERT INTO mem_tree_entity_index \
                    (entity_id, node_id, node_kind, owner_id, entity_kind, surface, score, \
                     timestamp_ms, tree_id, user_id) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
                 ON CONFLICT (owner_id, entity_id, node_id) DO UPDATE SET \
                     node_kind=EXCLUDED.node_kind, entity_kind=EXCLUDED.entity_kind, \
                     surface=EXCLUDED.surface, score=EXCLUDED.score, \
                     timestamp_ms=EXCLUDED.timestamp_ms, tree_id=EXCLUDED.tree_id, \
                     user_id=EXCLUDED.user_id",
                &[
                    &entry.entity_id, &entry.node_id, &entry.node_kind, &owner_id,
                    &entity_kind, &entry.surface, &score, &entry.timestamp_ms,
                    &entry.tree_id, &entry.user_id,
                ],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Get all node IDs associated with an entity.
    ///
    /// When `user_id` is `Some(u)`, only return nodes whose chunk is owned by
    /// `u` or owner-shared. `None` skips the filter.
    pub async fn chunks_for_entity(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        entity_id: &str,
        limit: usize,
    ) -> CarrierResult<Vec<(String, String)>> {
        let client = self.client().await?;
        let owner = owner_id.to_string();
        let eid = entity_id.to_string();
        let uid = user_id.map(str::to_string);
        let lim = limit as i64;

        let mut sql = "SELECT node_id, node_kind FROM mem_tree_entity_index \
                 WHERE owner_id=$1 AND entity_id=$2"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner, &eid];
        let mut i = 3;
        if let Some(u) = &uid {
            sql.push_str(&format!(" AND (user_id=${i} OR user_id='')"));
            params.push(u);
            i += 1;
        }
        sql.push_str(&format!(" ORDER BY timestamp_ms DESC LIMIT ${i}"));
        params.push(&lim);

        let rows = client
            .query(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter()
            .map(|r| -> CarrierResult<(String, String)> {
                let node_id = r
                    .try_get::<_, String>(0)
                    .map_err(|e| CarrierError::Serialization(e.to_string()))?;
                let node_kind = r
                    .try_get::<_, String>(1)
                    .map_err(|e| CarrierError::Serialization(e.to_string()))?;
                Ok((node_id, node_kind))
            })
            .collect()
    }

    /// List top entities for an owner by mention frequency.
    pub async fn top_entities(&self, owner_id: &str, limit: usize) -> CarrierResult<Vec<EntityMatch>> {
        let client = self.client().await?;
        let lim = limit as i64;
        let rows = client
            .query(
                "SELECT h.entity_id, i.entity_kind, i.surface, \
                        h.mention_count_30d::bigint, h.last_seen_ms \
                 FROM mem_tree_entity_hotness h \
                 LEFT JOIN mem_tree_entity_index i \
                   ON i.owner_id = h.owner_id AND i.entity_id = h.entity_id \
                 WHERE h.owner_id=$1 \
                 ORDER BY h.last_hotness DESC NULLS LAST \
                 LIMIT $2",
                &[&owner_id, &lim],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter().map(Self::row_to_entity_match).collect()
    }

    /// Fuzzy search entities by surface form.
    ///
    /// When `user_id` is `Some(u)`, only return entities from `u`'s chunks or
    /// owner-shared chunks. `None` skips the filter.
    pub async fn search_entities(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        query: &str,
        kind: Option<&EntityKind>,
        limit: usize,
    ) -> CarrierResult<Vec<EntityMatch>> {
        let client = self.client().await?;
        let owner = owner_id.to_string();
        let pattern = format!("%{query}%");
        let uid = user_id.map(str::to_string);
        let k = kind.map(|k| k.as_str().to_string());
        let lim = limit as i64;

        // PG (unlike SQLite) requires ORDER BY expressions to appear in the
        // SELECT list under DISTINCT. Group by the entity's identifying columns
        // (dedupe across nodes) and order by the best score per group.
        let mut sql = "SELECT entity_id, entity_kind, surface, 0::bigint as mc, 0::bigint as ls \
                     FROM mem_tree_entity_index \
                     WHERE owner_id=$1 AND surface LIKE $2 \
                     GROUP BY entity_id, entity_kind, surface"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner, &pattern];
        let mut i = 3;
        if let Some(u) = &uid {
            sql.push_str(&format!(" AND (user_id=${i} OR user_id='')"));
            params.push(u);
            i += 1;
        }
        if let Some(kv) = &k {
            sql.push_str(&format!(" AND entity_kind=${i}"));
            params.push(kv);
            i += 1;
        }
        sql.push_str(&format!(" ORDER BY MAX(score) DESC LIMIT ${i}"));
        params.push(&lim);

        let rows = client
            .query(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter().map(Self::row_to_entity_match).collect()
    }

    // -- Entity hotness ----------------------------------------------------

    /// Bump entity hotness counters after ingestion.
    ///
    /// Atomic upsert: on conflict increments mention_count_30d and
    /// ingests_since_check and refreshes last_seen/last_updated. `source_id`
    /// is currently unused (distinct_sources tracking is approximate in the
    /// source too - `let _ = source_id`).
    pub async fn bump_entity_hotness(
        &self,
        owner_id: &str,
        entity_id: &str,
        _source_id: &str,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        client
            .execute(
                "INSERT INTO mem_tree_entity_hotness \
                    (entity_id, owner_id, mention_count_30d, distinct_sources, \
                     last_seen_ms, query_hits_30d, graph_centrality, \
                     ingests_since_check, last_hotness, last_updated_ms) \
                 VALUES ($1,$2,1,1,$3,0,NULL,1,NULL,$3) \
                 ON CONFLICT (owner_id, entity_id) DO UPDATE SET \
                     mention_count_30d = mem_tree_entity_hotness.mention_count_30d + 1, \
                     last_seen_ms = EXCLUDED.last_seen_ms, \
                     ingests_since_check = mem_tree_entity_hotness.ingests_since_check + 1, \
                     last_updated_ms = EXCLUDED.last_updated_ms",
                &[&entity_id, &owner_id, &now_ms],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Get hotness counters for an entity.
    pub async fn get_hotness(
        &self,
        owner_id: &str,
        entity_id: &str,
    ) -> CarrierResult<Option<HotnessCounters>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT entity_id, mention_count_30d, distinct_sources, last_seen_ms, \
                        query_hits_30d, graph_centrality, ingests_since_check, \
                        last_hotness, last_updated_ms \
                 FROM mem_tree_entity_hotness WHERE owner_id=$1 AND entity_id=$2",
                &[&owner_id, &entity_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        match row {
            Some(r) => Ok(Some(Self::row_to_hotness(&r)?)),
            None => Ok(None),
        }
    }

    /// List hot entities above a threshold.
    pub async fn list_hot_entities(
        &self,
        owner_id: &str,
        threshold: f32,
        limit: usize,
    ) -> CarrierResult<Vec<HotnessCounters>> {
        let client = self.client().await?;
        let thr = threshold as f64;
        let lim = limit as i64;
        let rows = client
            .query(
                "SELECT entity_id, mention_count_30d, distinct_sources, last_seen_ms, \
                        query_hits_30d, graph_centrality, ingests_since_check, \
                        last_hotness, last_updated_ms \
                 FROM mem_tree_entity_hotness \
                 WHERE owner_id=$1 AND last_hotness >= $2 \
                 ORDER BY last_hotness DESC NULLS LAST \
                 LIMIT $3",
                &[&owner_id, &thr, &lim],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter().map(Self::row_to_hotness).collect()
    }

    /// Update the hotness score for an entity.
    pub async fn update_hotness_score(
        &self,
        owner_id: &str,
        entity_id: &str,
        hotness: f32,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let h = hotness as f64;
        client
            .execute(
                "UPDATE mem_tree_entity_hotness \
                 SET last_hotness=$1, ingests_since_check=0, last_updated_ms=$2 \
                 WHERE owner_id=$3 AND entity_id=$4",
                &[&h, &now_ms, &owner_id, &entity_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Get all entity IDs associated with a node (chunk or summary).
    ///
    /// When `user_id` is `Some(u)`, only return entities from `u`'s nodes or
    /// owner-shared nodes. `None` skips the filter.
    pub async fn entities_for_node(
        &self,
        owner_id: &str,
        user_id: Option<&str>,
        node_id: &str,
    ) -> CarrierResult<Vec<String>> {
        let client = self.client().await?;
        let owner = owner_id.to_string();
        let nid = node_id.to_string();
        let uid = user_id.map(str::to_string);

        let mut sql = "SELECT DISTINCT entity_id FROM mem_tree_entity_index \
                 WHERE owner_id=$1 AND node_id=$2"
            .to_string();
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&owner, &nid];
        if let Some(u) = &uid {
            sql.push_str(" AND (user_id=$3 OR user_id='')");
            params.push(u);
        }
        let rows = client
            .query(&sql, &params)
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        rows.iter()
            .map(|r| {
                r.try_get::<_, String>(0)
                    .map_err(|e| CarrierError::Serialization(e.to_string()))
            })
            .collect()
    }

    // -- Helpers -----------------------------------------------------------

    async fn client(&self) -> CarrierResult<deadpool_postgres::Object> {
        self.pool
            .get()
            .await
            .map_err(|e| CarrierError::Internal(format!("pg pool get: {e}")))
    }

    fn row_to_entity_match(row: &tokio_postgres::Row) -> CarrierResult<EntityMatch> {
        // entity_kind/surface come from a LEFT JOIN in top_entities -> may be NULL;
        // read as Option<String> then unwrap_or_default.
        let kind_str: String = row
            .try_get::<_, Option<String>>(1)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?
            .unwrap_or_default();
        let surface: String = row
            .try_get::<_, Option<String>>(2)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?
            .unwrap_or_default();
        // mention_count_30d is INT in top_entities (cast to ::bigint in SQL) and
        // `0::bigint` in search; both int8 here. last_seen_ms is nullable. Read
        // Option<i64> and unwrap_or(0) for uniformity across both queries.
        let mention_count: u64 = row
            .try_get::<_, Option<i64>>(3)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?
            .unwrap_or(0) as u64;
        let last_seen_ms: i64 = row
            .try_get::<_, Option<i64>>(4)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?
            .unwrap_or(0);
        Ok(EntityMatch {
            canonical_id: row
                .try_get::<_, String>(0)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            kind: Self::parse_entity_kind(&kind_str),
            surface,
            mention_count,
            last_seen_ms,
        })
    }

    fn row_to_hotness(row: &tokio_postgres::Row) -> CarrierResult<HotnessCounters> {
        // graph_centrality / last_hotness are DOUBLE PRECISION (f64); the struct
        // holds Option<f32>, so read Option<f64> and narrow.
        let graph_centrality: Option<f64> = row
            .try_get(5)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let last_hotness: Option<f64> = row
            .try_get(7)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        Ok(HotnessCounters {
            entity_id: row.try_get(0).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            mention_count_30d: row
                .try_get::<_, i32>(1)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            distinct_sources: row
                .try_get::<_, i32>(2)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            last_seen_ms: row
                .try_get(3)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            query_hits_30d: row
                .try_get::<_, i32>(4)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            graph_centrality: graph_centrality.map(|v| v as f32),
            ingests_since_check: row
                .try_get::<_, i32>(6)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            last_hotness: last_hotness.map(|v| v as f32),
            last_updated_ms: row
                .try_get(8)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }

    pub fn parse_entity_kind(s: &str) -> EntityKind {
        match s {
            "email" => EntityKind::Email,
            "url" => EntityKind::Url,
            "handle" => EntityKind::Handle,
            "hashtag" => EntityKind::Hashtag,
            "person" => EntityKind::Person,
            "organization" => EntityKind::Organization,
            "location" => EntityKind::Location,
            "event" => EntityKind::Event,
            "product" => EntityKind::Product,
            "datetime" => EntityKind::Datetime,
            "technology" => EntityKind::Technology,
            "artifact" => EntityKind::Artifact,
            "quantity" => EntityKind::Quantity,
            "misc" => EntityKind::Misc,
            "topic" => EntityKind::Topic,
            _ => EntityKind::Misc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;

    async fn setup() -> Option<EntityStore> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        Some(EntityStore::new(pool))
    }

    fn entry<'a>(entity_id: &'a str, node_id: &'a str, surface: &'a str, score: f32) -> EntityIndexEntry<'a> {
        EntityIndexEntry {
            entity_id,
            node_id,
            node_kind: "leaf",
            entity_kind: EntityKind::Person,
            surface,
            score,
            timestamp_ms: 1000,
            tree_id: Some("tree_1"),
            user_id: "",
        }
    }

    #[tokio::test]
    async fn upsert_and_chunks_for_entity() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        store
            .upsert_entity_index("owner_1", &entry("person:Alice", "chunk_001", "Alice", 0.8))
            .await
            .unwrap();
        let nodes = store.chunks_for_entity("owner_1", None, "person:Alice", 10).await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].0, "chunk_001");
    }

    #[tokio::test]
    async fn bump_entity_hotness() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.bump_entity_hotness("owner_1", "person:Alice", "source_1").await.unwrap();
        store.bump_entity_hotness("owner_1", "person:Alice", "source_1").await.unwrap();
        let hotness = store.get_hotness("owner_1", "person:Alice").await.unwrap().unwrap();
        assert_eq!(hotness.mention_count_30d, 2);
    }

    #[tokio::test]
    async fn update_hotness_score() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.bump_entity_hotness("owner_1", "person:Alice", "source_1").await.unwrap();
        store.update_hotness_score("owner_1", "person:Alice", 15.0).await.unwrap();
        let hotness = store.get_hotness("owner_1", "person:Alice").await.unwrap().unwrap();
        assert!((hotness.last_hotness.unwrap() - 15.0).abs() < 0.01);
        assert_eq!(hotness.ingests_since_check, 0);
    }

    #[tokio::test]
    async fn list_hot_entities() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.bump_entity_hotness("owner_1", "person:Alice", "source_1").await.unwrap();
        store.bump_entity_hotness("owner_1", "person:Bob", "source_1").await.unwrap();
        store.update_hotness_score("owner_1", "person:Alice", 15.0).await.unwrap();
        store.update_hotness_score("owner_1", "person:Bob", 5.0).await.unwrap();
        let hot = store.list_hot_entities("owner_1", 10.0, 10).await.unwrap();
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].entity_id, "person:Alice");
    }

    #[tokio::test]
    async fn search_entities() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store
            .upsert_entity_index("owner_1", &entry("person:Alice", "chunk_001", "Alice Smith", 0.8))
            .await
            .unwrap();
        store
            .upsert_entity_index("owner_1", &entry("person:Bob", "chunk_002", "Bob Jones", 0.6))
            .await
            .unwrap();
        let results = store.search_entities("owner_1", None, "Alice", None, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].canonical_id, "person:Alice");
    }

    #[tokio::test]
    async fn entities_for_node_and_user_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        // Alice-only entry.
        let mut alice_entry = entry("person:Alice", "chunk_alice", "Alice", 0.8);
        alice_entry.user_id = "alice";
        store.upsert_entity_index("owner_1", &alice_entry).await.unwrap();
        // Owner-shared entry.
        store
            .upsert_entity_index("owner_1", &entry("person:Bob", "chunk_shared", "Bob", 0.6))
            .await
            .unwrap();

        // Bob sees only the owner-shared entity for the shared node, none for alice's node.
        let bob_for_alice = store.entities_for_node("owner_1", Some("bob"), "chunk_alice").await.unwrap();
        assert!(bob_for_alice.is_empty());
        let bob_for_shared = store.entities_for_node("owner_1", Some("bob"), "chunk_shared").await.unwrap();
        assert_eq!(bob_for_shared, vec!["person:Bob".to_string()]);
        // Alice sees her own.
        let alice_own = store.entities_for_node("owner_1", Some("alice"), "chunk_alice").await.unwrap();
        assert_eq!(alice_own, vec!["person:Alice".to_string()]);
        // None (write-path) sees all.
        let all = store.entities_for_node("owner_1", None, "chunk_alice").await.unwrap();
        assert_eq!(all, vec!["person:Alice".to_string()]);
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
        store
            .upsert_entity_index("owner_1", &entry("person:Alice", "chunk_001", "Alice", 0.8))
            .await
            .unwrap();
        let results = store.search_entities("owner_2", None, "Alice", None, 10).await.unwrap();
        assert!(results.is_empty());
    }
}
