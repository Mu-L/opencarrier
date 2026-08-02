//! PG-backed score store - mirrors `memory::tree::score_store::ScoreStore`.
//!
//! `mem_tree_score` is an owner-level aggregate (no `user_id`). All `f32`
//! signal/total/hotness values bind to and read from DOUBLE PRECISION as `f64`
//! (tokio-postgres maps `f32` to FLOAT4, which mismatches FLOAT8 columns).
//! Reuses `ScoreRow` / `ScoreSignals` from the memory crate so the jobs layer
//! can swap stores without type drift.

use deadpool_postgres::Pool;
use memory::tree::score_store::ScoreRow;
use memory::tree::types::ScoreSignals;
use types::error::{CarrierError, CarrierResult};

/// Score store backed by PG.
pub struct ScoreStore {
    pool: Pool,
}

impl ScoreStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Write a score row for a chunk (insert or replace by chunk_id PK).
    ///
    /// `reason` is bound to both `llm_importance_reason` and `reason`, matching
    /// the SQLite store's behaviour exactly (ports the source binding order;
    /// not "fixed" to avoid behaviour drift).
    pub async fn write_score(
        &self,
        owner_id: &str,
        chunk_id: &str,
        signals: &ScoreSignals,
        total: f32,
        dropped: bool,
        reason: Option<&str>,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        // Promote every f32 signal to f64 so it binds to DOUBLE PRECISION.
        let total = total as f64;
        let tc = signals.token_count as f64;
        let uw = signals.unique_words as f64;
        let mw = signals.metadata_weight as f64;
        let sw = signals.source_weight as f64;
        let iw = signals.interaction as f64;
        let ed = signals.entity_density as f64;
        let li = signals.llm_importance as f64;

        client
            .execute(
                "INSERT INTO mem_tree_score \
                    (chunk_id, owner_id, total, token_count_signal, unique_words_signal, \
                     metadata_weight, source_weight, interaction_weight, entity_density, \
                     llm_importance, llm_importance_reason, dropped, reason, computed_at_ms) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) \
                 ON CONFLICT (chunk_id) DO UPDATE SET \
                     owner_id=EXCLUDED.owner_id, total=EXCLUDED.total, \
                     token_count_signal=EXCLUDED.token_count_signal, \
                     unique_words_signal=EXCLUDED.unique_words_signal, \
                     metadata_weight=EXCLUDED.metadata_weight, \
                     source_weight=EXCLUDED.source_weight, \
                     interaction_weight=EXCLUDED.interaction_weight, \
                     entity_density=EXCLUDED.entity_density, \
                     llm_importance=EXCLUDED.llm_importance, \
                     llm_importance_reason=EXCLUDED.llm_importance_reason, \
                     dropped=EXCLUDED.dropped, reason=EXCLUDED.reason, \
                     computed_at_ms=EXCLUDED.computed_at_ms",
                &[
                    &chunk_id, &owner_id, &total, &tc, &uw, &mw, &sw, &iw, &ed, &li,
                    &reason, &dropped, &reason, &now_ms,
                ],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Get the score for a chunk.
    pub async fn get_score(
        &self,
        owner_id: &str,
        chunk_id: &str,
    ) -> CarrierResult<Option<ScoreRow>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT token_count_signal, unique_words_signal, metadata_weight, \
                        source_weight, interaction_weight, entity_density, llm_importance, \
                        total, dropped, reason \
                 FROM mem_tree_score WHERE owner_id=$1 AND chunk_id=$2",
                &[&owner_id, &chunk_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        match row {
            Some(r) => Ok(Some(Self::row_to_score(&r)?)),
            None => Ok(None),
        }
    }

    /// Get the LLM importance for a chunk (used for re-scoring).
    pub async fn get_llm_importance(
        &self,
        owner_id: &str,
        chunk_id: &str,
    ) -> CarrierResult<Option<(f32, Option<String>)>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT llm_importance, llm_importance_reason \
                 FROM mem_tree_score WHERE owner_id=$1 AND chunk_id=$2",
                &[&owner_id, &chunk_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        match row {
            Some(r) => {
                let importance: f64 = r
                    .try_get(0)
                    .map_err(|e| CarrierError::Serialization(e.to_string()))?;
                let reason: Option<String> = r
                    .try_get(1)
                    .map_err(|e| CarrierError::Serialization(e.to_string()))?;
                Ok(Some((importance as f32, reason)))
            }
            None => Ok(None),
        }
    }

    /// Update the LLM importance for a chunk.
    pub async fn set_llm_importance(
        &self,
        owner_id: &str,
        chunk_id: &str,
        importance: f32,
        reason: Option<&str>,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let imp = importance as f64;
        client
            .execute(
                "UPDATE mem_tree_score SET llm_importance=$1, llm_importance_reason=$2 \
                 WHERE owner_id=$3 AND chunk_id=$4",
                &[&imp, &reason, &owner_id, &chunk_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    async fn client(&self) -> CarrierResult<deadpool_postgres::Object> {
        self.pool
            .get()
            .await
            .map_err(|e| CarrierError::Internal(format!("pg pool get: {e}")))
    }

    fn row_to_score(row: &tokio_postgres::Row) -> CarrierResult<ScoreRow> {
        let g = |i: usize| -> CarrierResult<f64> {
            row.try_get(i).map_err(|e| CarrierError::Serialization(e.to_string()))
        };
        Ok(ScoreRow {
            signals: ScoreSignals {
                token_count: g(0)? as f32,
                unique_words: g(1)? as f32,
                metadata_weight: g(2)? as f32,
                source_weight: g(3)? as f32,
                interaction: g(4)? as f32,
                entity_density: g(5)? as f32,
                llm_importance: g(6)? as f32,
            },
            total: g(7)? as f32,
            dropped: row
                .try_get(8)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            reason: row
                .try_get(9)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;

    async fn setup() -> Option<ScoreStore> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        Some(ScoreStore::new(pool))
    }

    fn signals() -> ScoreSignals {
        ScoreSignals {
            token_count: 0.5,
            unique_words: 0.6,
            metadata_weight: 0.7,
            source_weight: 0.8,
            interaction: 0.9,
            entity_density: 0.4,
            llm_importance: 0.0,
        }
    }

    #[tokio::test]
    async fn write_and_get_score() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        store
            .write_score("owner_1", "chunk_001", &signals(), 0.75, false, Some("high quality"))
            .await
            .unwrap();
        let row = store.get_score("owner_1", "chunk_001").await.unwrap().unwrap();
        assert!((row.total - 0.75).abs() < 0.01);
        assert!(!row.dropped);
        assert_eq!(row.reason, Some("high quality".to_string()));
        assert!((row.signals.token_count - 0.5).abs() < 0.01);
        // write_score binds reason to llm_importance_reason too (source behaviour).
        assert_eq!(row.signals.llm_importance, 0.0);
    }

    #[tokio::test]
    async fn get_missing_score() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        assert!(store.get_score("owner_1", "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_llm_importance() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store
            .write_score("owner_1", "chunk_001", &ScoreSignals::default(), 0.5, false, None)
            .await
            .unwrap();
        store
            .set_llm_importance("owner_1", "chunk_001", 0.9, Some("very important"))
            .await
            .unwrap();
        let (importance, reason) = store
            .get_llm_importance("owner_1", "chunk_001")
            .await
            .unwrap()
            .unwrap();
        assert!((importance - 0.9).abs() < 0.01);
        assert_eq!(reason, Some("very important".to_string()));
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
            .write_score("owner_1", "chunk_001", &ScoreSignals::default(), 0.5, false, None)
            .await
            .unwrap();
        assert!(store.get_score("owner_2", "chunk_001").await.unwrap().is_none());
    }
}
