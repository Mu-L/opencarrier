//! Worker pool: claims jobs from `mem_tree_jobs`, dispatches them through
//! [`handlers::handle_job`], and settles the row.
//!
//! Ported from `memory::tree::jobs::worker` - same loop, but backed by the PG
//! async pool. Two differences from the source:
//!
//! - No `spawn_blocking`: the source wrapped the sync SQLite `handle_job` in
//!   `spawn_blocking`; here `handle_job` is already async, so the worker task
//!   awaits it directly.
//! - `worker_count` is a field (not a hard-coded `const 4`) so stage-5 graded
//!   activation can start at 0 (drain-safe) and ramp to 1 -> 4.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use tokio::sync::Notify;

use super::handlers::{handle_job, JobOutcome};
use crate::pg::job_store::JobStore;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Worker pool that polls the job queue and dispatches handlers.
pub struct TreeWorkerPool {
    pool: Pool,
    content_root: PathBuf,
    worker_count: usize,
    notify: Arc<Notify>,
}

impl TreeWorkerPool {
    /// `worker_count = 0` is a safe first-activation mode: the pool recovers
    /// stale locks on `start` but spawns no consumers, so jobs queue without
    /// being processed until the count is raised.
    pub fn new(pool: Pool, content_root: PathBuf, worker_count: usize) -> Self {
        Self {
            pool,
            content_root,
            worker_count,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Start the worker pool. Recovers stale locks, then spawns `worker_count`
    /// tokio tasks (0 = no consumers, queue-only).
    pub async fn start(self: &Arc<Self>) {
        // Recover stale locks at startup.
        let job_store = JobStore::new(self.pool.clone());
        if let Err(e) = job_store.recover_stale_locks().await {
            tracing::warn!("[tree_jobs] recover_stale_locks failed at startup: {e:#}");
        }

        for idx in 0..self.worker_count {
            let pool = Arc::clone(self);
            let notify = self.notify.clone();
            tokio::spawn(async move {
                loop {
                    match pool.run_once().await {
                        Ok(true) => continue,
                        Ok(false) => {
                            tokio::select! {
                                _ = notify.notified() => {}
                                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                            }
                        }
                        Err(e) => {
                            tracing::warn!("[tree_jobs] worker {idx} error: {e:#}");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            });
        }

        // Always-on reaper: bounds `mem_tree_jobs` growth even at worker_count=0
        // (the shipped default) and with the scheduler off. Without it, every
        // ingest's ExtractChunk job plus all done/failed rows accumulate forever.
        // Deletes done/failed rows older than 24h, hourly. Runs regardless of
        // worker_count (the pool always calls `start`, even with 0 consumers).
        {
            let pool = self.pool.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    ticker.tick().await;
                    let store = JobStore::new(pool.clone());
                    let cutoff = chrono::Utc::now().timestamp_millis() - 86_400_000; // 24h
                    match store.delete_settled_before(cutoff).await {
                        Ok(n) if n > 0 => {
                            tracing::info!(
                                "[tree_jobs] reaper deleted {n} settled jobs older than 24h"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("[tree_jobs] reaper error: {e:#}"),
                    }
                }
            });
        }

        if self.worker_count == 0 {
            tracing::warn!(
                "[tree_jobs] worker pool started with 0 consumers (queue-only mode): \
                 tree memory recall will return EMPTY until AGINX_MEMORY_WORKER_COUNT \
                 is raised above 0 - ingest stores chunks but no worker seals summaries"
            );
        }
    }

    /// Wake idle workers so they re-poll immediately.
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }

    /// Claim and run a single job. Returns `true` when work was processed.
    async fn run_once(&self) -> Result<bool, types::error::CarrierError> {
        let job_store = JobStore::new(self.pool.clone());
        let Some(job) = job_store.claim_next(None).await? else {
            return Ok(false);
        };

        let job_id = job.id.clone();

        // handle_job is async (PG) - no spawn_blocking needed.
        match handle_job(&self.pool, &self.content_root, &job.owner_id, &job).await {
            Ok(JobOutcome::Done) => {
                if let Err(e) = job_store.mark_done(&job_id, job.locked_until_ms).await {
                    tracing::warn!("[tree_jobs] mark_done failed for {job_id}: {e:#}");
                }
            }
            Ok(JobOutcome::Defer { until_ms, .. }) => {
                if let Err(e) = job_store.defer(&job_id, until_ms).await {
                    tracing::warn!("[tree_jobs] defer failed for {job_id}: {e:#}");
                }
            }
            Err(e) => {
                tracing::warn!("[tree_jobs] job failed id={job_id} err={e:#}");
                if let Err(e2) =
                    job_store.mark_failed(&job_id, &format!("{e:#}"), job.locked_until_ms).await
                {
                    tracing::warn!("[tree_jobs] mark_failed error: {e2:#}");
                }
            }
        }

        Ok(true)
    }
}
