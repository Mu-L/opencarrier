//! PostgreSQL storage layer for aginxMemory.
//!
//! Mirrors the kv+tree stores from `memory::system_kv` / `memory::tree::*` but
//! backed by PG (tokio-postgres + deadpool-postgres) instead of rusqlite. Runtime
//! state (sessions/agents/cron/...) stays in opencarrier's in-process SQLite and
//! is NOT here.

pub mod chunk_store;
pub mod entity_store;
pub mod job_store;
pub mod kv_store;
pub mod score_store;
pub mod tree_store;

#[cfg(test)]
/// Drop all aginxMemory tables and re-run migrations. Tests call this so each
/// run starts from a clean schema (V1 edits during dev don't fight `IF NOT
/// EXISTS` on an already-migrated DB).
pub(crate) async fn reset_and_migrate(client: &mut tokio_postgres::Client) {
    let _ = client
        .batch_execute(
            "DROP TABLE IF EXISTS kv_store, kv_history, mem_tree_chunks, mem_tree_score, \
             mem_tree_entity_index, mem_tree_trees, mem_tree_summaries, mem_tree_buffers, \
             mem_tree_entity_hotness, mem_tree_jobs, mem_tree_ingested_sources, \
             refinery_schema_history CASCADE",
        )
        .await;
    let _ = crate::migrations::runner().run_async(client).await;
}
