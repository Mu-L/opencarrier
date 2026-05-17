//! Worker pool: claims jobs from `mem_tree_jobs`, dispatches them through
//! [`handlers::handle_job`], and settles the row.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::Notify;

use super::handlers::{handle_job, JobOutcome};
use crate::tree::job_store::JobStore;

const WORKER_COUNT: usize = 4;
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Worker pool that polls the job queue and dispatches handlers.
pub struct TreeWorkerPool {
    conn: Arc<Mutex<Connection>>,
    content_root: PathBuf,
    notify: Arc<Notify>,
}

impl TreeWorkerPool {
    pub fn new(conn: Arc<Mutex<Connection>>, content_root: PathBuf) -> Self {
        Self {
            conn,
            content_root,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Start the worker pool. Spawns `WORKER_COUNT` tokio tasks.
    pub fn start(self: &Arc<Self>) {
        // Recover stale locks at startup
        let job_store = JobStore::new(self.conn.clone());
        if let Err(e) = job_store.recover_stale_locks() {
            tracing::warn!("[tree_jobs] recover_stale_locks failed at startup: {e:#}");
        }

        for idx in 0..WORKER_COUNT {
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
    }

    /// Wake idle workers so they re-poll immediately.
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }

    /// Claim and run a single job. Returns `true` when work was processed.
    async fn run_once(&self) -> Result<bool, types::error::CarrierError> {
        let job_store = JobStore::new(self.conn.clone());
        let Some(job) = job_store.claim_next(None)? else {
            return Ok(false);
        };

        let conn = self.conn.clone();
        let content_root = self.content_root.clone();
        let job_id = job.id.clone();

        // Run handler in a blocking context (SQLite operations are sync)
        let result = tokio::task::spawn_blocking(move || {
            handle_job(&conn, &content_root, &job.owner_id, &job)
        })
        .await;

        match result {
            Ok(Ok(outcome)) => match outcome {
                JobOutcome::Done => {
                    if let Err(e) = job_store.mark_done(&job_id) {
                        tracing::warn!("[tree_jobs] mark_done failed for {job_id}: {e:#}");
                    }
                }
                JobOutcome::Defer { until_ms, .. } => {
                    if let Err(e) = job_store.defer(&job_id, until_ms) {
                        tracing::warn!("[tree_jobs] defer failed for {job_id}: {e:#}");
                    }
                }
            },
            Ok(Err(e)) => {
                tracing::warn!(
                    "[tree_jobs] job failed id={job_id} err={e:#}"
                );
                if let Err(e2) = job_store.mark_failed(&job_id, &format!("{e:#}")) {
                    tracing::warn!("[tree_jobs] mark_failed error: {e2:#}");
                }
            }
            Err(e) => {
                tracing::warn!("[tree_jobs] spawn_blocking panic for {job_id}: {e:#}");
                let _ = job_store.mark_failed(&job_id, "worker panic");
            }
        }

        Ok(true)
    }
}
