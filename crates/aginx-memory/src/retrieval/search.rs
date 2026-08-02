//! Fuzzy entity search over the entity index (PG-backed).

use deadpool_postgres::Pool;
use memory::tree::types::EntityKind;
use types::error::CarrierResult;
use types::memory_tree::EntityMatch;

use crate::pg::entity_store::EntityStore;

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 100;

/// Search entities by substring match on canonical_id or surface form.
///
/// When `user_id` is `Some(u)`, only entities from `u`'s chunks or
/// owner-shared chunks are returned.
pub async fn search_entities(
    pool: &Pool,
    owner_id: &str,
    user_id: Option<&str>,
    query: &str,
    kind: Option<EntityKind>,
    limit: usize,
) -> CarrierResult<Vec<EntityMatch>> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit.min(MAX_LIMIT) };
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let entity_store = EntityStore::new(pool.clone());
    entity_store
        .search_entities(owner_id, user_id, query, kind.as_ref(), limit)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;
    use memory::tree::entity_store::EntityIndexEntry;

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

    #[tokio::test]
    async fn empty_query_returns_empty() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let result = search_entities(&pool, "owner_1", None, "", None, 10).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn search_after_index() {
        let pool = match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let entity_store = EntityStore::new(pool.clone());
        let entry = EntityIndexEntry {
            entity_id: "email:alice@example.com",
            node_id: "chunk_1",
            node_kind: "leaf",
            entity_kind: EntityKind::Email,
            surface: "alice@example.com",
            score: 0.5,
            timestamp_ms: 1_700_000_000_000,
            tree_id: None,
            user_id: "",
        };
        entity_store.upsert_entity_index("owner_1", &entry).await.unwrap();

        let result = search_entities(&pool, "owner_1", None, "alice", None, 10).await.unwrap();
        assert!(!result.is_empty());
        assert!(result[0].canonical_id.contains("alice"));
    }
}
