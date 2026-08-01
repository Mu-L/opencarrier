//! PostgreSQL storage layer for aginxMemory.
//!
//! Mirrors the kv+tree stores from `memory::system_kv` / `memory::tree::*` but
//! backed by PG (tokio-postgres + deadpool-postgres) instead of rusqlite. Runtime
//! state (sessions/agents/cron/...) stays in opencarrier's in-process SQLite and
//! is NOT here.

pub mod kv_store;
