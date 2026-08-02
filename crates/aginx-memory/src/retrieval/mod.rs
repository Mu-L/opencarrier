//! Retrieval primitives for the tree memory system (PG-backed).
//!
//! Ported from `memory::tree::retrieval` - same six read-only primitives,
//! stores swapped to the PG async stores and every primitive is `async`:
//! - `query_source` - per-source summary retrieval
//! - `query_global` - cross-source digest for a time window
//! - `query_topic` - entity-scoped retrieval
//! - `search_entities` - fuzzy canonical-id lookup
//! - `drill_down` - walk summary children (BFS)
//! - `fetch_leaves` - batch chunk hydration

pub mod drill_down;
pub mod fetch;
pub mod global;
pub mod search;
pub mod source;
pub mod topic;
