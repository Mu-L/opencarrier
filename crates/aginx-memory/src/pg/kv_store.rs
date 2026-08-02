//! PG-backed kv store - mirrors `memory::system_kv::SystemKV`'s kv operations
//! (agent entries stay in opencarrier's in-process SQLite; this owns only
//! `kv_store` + `kv_history`).
//!
//! `value` is stored as PG `JSONB` (the SQLite version used `BLOB` =
//! `serde_json::to_vec`; tokio-postgres' `with-serde_json-1` feature maps
//! `serde_json::Value` <-> JSONB directly). Set/delete archive the previous
//! value to `kv_history` in a transaction (memory immutability guarantee).

use deadpool_postgres::Pool;
use serde_json::Value;
use types::error::{CarrierError, CarrierResult};

/// PG-backed key-value store scoped by `(agent_id, owner_id, user_id, key)`.
#[derive(Clone)]
pub struct KvStore {
    pool: Pool,
}

impl KvStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    pub async fn get(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<Option<Value>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT value FROM kv_store \
                 WHERE agent_id=$1 AND owner_id=$2 AND user_id=$3 AND key=$4",
                &[&agent_id, &owner_id, &user_id, &key],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(row.map(|r| r.get::<_, Value>(0)))
    }

    pub async fn set(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: Value,
    ) -> CarrierResult<()> {
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        // Archive the previous value before overwriting (memory immutability).
        let old: Option<(Value, i32)> = tx
            .query_opt(
                "SELECT value, version FROM kv_store \
                 WHERE agent_id=$1 AND owner_id=$2 AND user_id=$3 AND key=$4",
                &[&agent_id, &owner_id, &user_id, &key],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?
            .map(|r| (r.get(0), r.get(1)));

        let now = chrono::Utc::now().to_rfc3339();
        if let Some((old_val, old_ver)) = old {
            tx.execute(
                "INSERT INTO kv_history \
                    (agent_id, owner_id, user_id, key, value, version, archived_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7)",
                &[&agent_id, &owner_id, &user_id, &key, &old_val, &old_ver, &now],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }

        tx.execute(
            "INSERT INTO kv_store (agent_id, owner_id, user_id, key, value, version, updated_at) \
             VALUES ($1,$2,$3,$4,$5,1,$6) \
             ON CONFLICT (agent_id, owner_id, user_id, key) \
             DO UPDATE SET value=$5, version=kv_store.version+1, updated_at=$6",
            &[&agent_id, &owner_id, &user_id, &key, &value, &now],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;

        tx.commit().await.map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    pub async fn delete(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<()> {
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let old: Option<(Value, i32)> = tx
            .query_opt(
                "SELECT value, version FROM kv_store \
                 WHERE agent_id=$1 AND owner_id=$2 AND user_id=$3 AND key=$4",
                &[&agent_id, &owner_id, &user_id, &key],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?
            .map(|r| (r.get(0), r.get(1)));

        let now = chrono::Utc::now().to_rfc3339();
        if let Some((old_val, old_ver)) = old {
            tx.execute(
                "INSERT INTO kv_history \
                    (agent_id, owner_id, user_id, key, value, version, archived_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7)",
                &[&agent_id, &owner_id, &user_id, &key, &old_val, &old_ver, &now],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }

        tx.execute(
            "DELETE FROM kv_store \
             WHERE agent_id=$1 AND owner_id=$2 AND user_id=$3 AND key=$4",
            &[&agent_id, &owner_id, &user_id, &key],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;

        tx.commit().await.map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    pub async fn list_kv(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> CarrierResult<Vec<(String, Value)>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT key, value FROM kv_store \
                 WHERE agent_id=$1 AND owner_id=$2 AND user_id=$3 ORDER BY key",
                &[&agent_id, &owner_id, &user_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    async fn client(&self) -> CarrierResult<deadpool_postgres::Object> {
        self.pool
            .get()
            .await
            .map_err(|e| CarrierError::Internal(format!("pg pool get: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a pool from `AGINX_MEMORY_TEST_PG` (e.g.
    /// `postgres://test@localhost:5433/aginx_memory`) and apply schema. Returns
    /// None if the env var is unset -> tests skip (no PG available).
    async fn setup() -> Option<KvStore> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        // Migrate via a direct connection: refinery's `run_async` needs
        // `&mut tokio_postgres::Client` (impl `AsyncMigrate`), but deadpool's
        // `Object` derefs to a `ClientWrapper` that doesn't impl it.
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        // KvStore uses a deadpool pool for concurrent queries.
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = deadpool_postgres::Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        Some(KvStore::new(pool))
    }

    #[tokio::test]
    async fn kv_set_get() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        store
            .set("a", "o", "u", "k", json!("test_value"))
            .await
            .unwrap();
        assert_eq!(store.get("a", "o", "u", "k").await.unwrap(), Some(json!("test_value")));
    }

    #[tokio::test]
    async fn kv_get_missing() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        assert_eq!(store.get("a", "o", "u", "nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn kv_delete() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.set("a", "o", "u", "del", json!(42)).await.unwrap();
        store.delete("a", "o", "u", "del").await.unwrap();
        assert_eq!(store.get("a", "o", "u", "del").await.unwrap(), None);
    }

    #[tokio::test]
    async fn kv_update() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.set("a", "o", "u", "k", json!("v1")).await.unwrap();
        store.set("a", "o", "u", "k", json!("v2")).await.unwrap();
        assert_eq!(store.get("a", "o", "u", "k").await.unwrap(), Some(json!("v2")));
    }

    #[tokio::test]
    async fn kv_per_user_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.set("a", "owner", "user_a", "pref", json!("dark")).await.unwrap();
        store.set("a", "owner", "user_b", "pref", json!("light")).await.unwrap();
        assert_eq!(
            store.get("a", "owner", "user_a", "pref").await.unwrap(),
            Some(json!("dark"))
        );
        assert_eq!(
            store.get("a", "owner", "user_b", "pref").await.unwrap(),
            Some(json!("light"))
        );
        assert_eq!(store.list_kv("a", "owner", "user_a").await.unwrap().len(), 1);
        assert_eq!(store.list_kv("a", "owner", "user_b").await.unwrap().len(), 1);
    }
}
