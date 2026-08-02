//! Tree memory job pipeline: per-kind handlers + worker pool + daily scheduler.
//!
//! Ported from `memory::tree::jobs` - same structure, backed by the PG async
//! pool. The worker consumes `mem_tree_jobs` (claimed atomically via
//! `FOR UPDATE SKIP LOCKED` in [`crate::pg::job_store`]) and dispatches each job
//! to [`handlers::handle_job`].

pub mod handlers;
pub mod scheduler;
pub mod worker;

pub use handlers::{handle_job, JobOutcome};
pub use worker::TreeWorkerPool;
