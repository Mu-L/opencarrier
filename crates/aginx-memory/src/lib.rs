//! aginxMemory library - PG-backed kv+tree memory store.
//!
//! The `aginx-memory` bin (src/main.rs) is the HTTP daemon; this lib holds the
//! storage layer (`pg::*`) reused by the daemon and by integration tests.

// refinery embeds the `migrations/` dir (V<n>__<name>.sql files) at compile time,
// generating a `migrations` module with a `runner()` function. Used by the bin
// on startup and by tests to set up schema.
refinery::embed_migrations!("migrations");

pub mod pg;
pub mod bucket_seal;
pub mod routing;
pub mod digest;
pub mod jobs;
