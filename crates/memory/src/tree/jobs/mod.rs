//! Background job system for tree memory work.
//!
//! A SQLite-backed job queue with a tokio worker pool. The pipeline:
//!
//! ```text
//! ingest → persists chunk → enqueues ExtractChunk
//!
//! worker pool (4 tasks) → claims jobs:
//!   ExtractChunk   → entity extraction → admission → enqueue AppendBuffer
//!   AppendBuffer   → push to L0 buffer → enqueue Seal if gate met
//!   Seal           → seal one level → enqueue parent Seal if cascading
//!   TopicRoute     → match topics → enqueue per-topic AppendBuffer
//!   DigestDaily    → call tree_global::digest::end_of_day_digest
//!   FlushStale     → enqueue Seal jobs for time-stale buffers
//!
//! scheduler (1 task) → daily wall-clock tick:
//!   enqueues DigestDaily(yesterday) + FlushStale(today)
//! ```

pub mod handlers;
pub mod scheduler;
pub mod worker;
