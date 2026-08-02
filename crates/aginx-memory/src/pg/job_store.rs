//! PG-backed job queue store - mirrors `memory::tree::job_store::JobStore`.
//!
//! Owns `mem_tree_jobs` (owner-level async pipeline queue, no user_id) and
//! `mem_tree_ingested_sources` (owner-level ingest dedupe). Two atomicity
//! upgrades over the SQLite source:
//!
//! - `enqueue` dedupe: the source does SELECT-COUNT-then-INSERT, which races
//!   under a multi-connection pool (two callers both see count 0, both insert).
//!   Here we rely on the partial unique index
//!   `idx_mem_tree_jobs_owner_dedupe_active` and `ON CONFLICT DO NOTHING`; if the
//!   insert reports 0 rows affected, an active dedupe match exists -> None.
//! - `claim_next`: the source does SELECT-then-UPDATE (non-atomic; two workers
//!   can both grab the same row). Here we `SELECT ... FOR UPDATE SKIP LOCKED`
//!   inside a transaction so concurrent workers each claim a distinct row.
//!
//! Reuses `Job` / `JobKind` / `JobStatus` / `NewJob` from the memory crate.

use deadpool_postgres::Pool;
use memory::tree::types::{Job, JobKind, JobStatus, NewJob};
use types::error::{CarrierError, CarrierResult};

/// Lock duration for claimed jobs (5 minutes).
const LOCK_DURATION_MS: i64 = 300_000;

/// Job store backed by PG.
#[derive(Clone)]
pub struct JobStore {
    pool: Pool,
}

impl JobStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Enqueue a new job. Returns `Some(job_id)` on success, `None` if deduped
    /// (an active ready/running job with the same `(owner_id, dedupe_key)`
    /// already exists).
    pub async fn enqueue(&self, job: &NewJob) -> CarrierResult<Option<String>> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let job_id = format!("job_{}", uuid::Uuid::new_v4().simple());
        let kind = job.kind.as_str().to_string();
        let available_at_ms = job.available_at_ms.unwrap_or(now_ms);
        let max_attempts = job.max_attempts.unwrap_or(5) as i32;

        // Atomic dedupe: the partial unique index
        // (owner_id, dedupe_key) WHERE dedupe_key IS NOT NULL AND status IN
        // ('ready','running') arbitrates - 0 rows affected means a conflict was
        // suppressed (active dedupe match exists).
        let affected = client
            .execute(
                "INSERT INTO mem_tree_jobs \
                    (id, owner_id, kind, payload_json, dedupe_key, status, attempts, \
                     max_attempts, available_at_ms, locked_until_ms, last_error, \
                     created_at_ms, started_at_ms, completed_at_ms) \
                 VALUES ($1,$2,$3,$4,$5,'ready',0,$6,$7,NULL,NULL,$8,NULL,NULL) \
                 ON CONFLICT (owner_id, dedupe_key) \
                     WHERE dedupe_key IS NOT NULL AND status IN ('ready','running') \
                 DO NOTHING",
                &[
                    &job_id, &job.owner_id, &kind, &job.payload_json, &job.dedupe_key,
                    &max_attempts, &available_at_ms, &now_ms,
                ],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        if affected == 0 {
            Ok(None)
        } else {
            Ok(Some(job_id))
        }
    }

    /// Claim the next ready job for an owner (or any owner if `None`).
    ///
    /// Uses `SELECT ... FOR UPDATE SKIP LOCKED` inside a transaction so
    /// concurrent workers each claim a distinct row (the SQLite store's
    /// select-then-update can hand the same row to two workers).
    pub async fn claim_next(&self, owner_id: Option<&str>) -> CarrierResult<Option<Job>> {
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let locked_until = now_ms + LOCK_DURATION_MS;

        let cols = "id, owner_id, kind, payload_json, dedupe_key, status, \
                   attempts, max_attempts, available_at_ms, locked_until_ms, \
                   last_error, created_at_ms, started_at_ms, completed_at_ms";
        // `owner_id = $N` must sit in the WHERE clause, before ORDER BY / LIMIT /
        // FOR UPDATE - so the two branches are fully separate SQL strings (the
        // param order differs: $1=owner vs $1=now).
        let row = match owner_id {
            Some(oid) => {
                tx.query_opt(
                    &format!(
                        "SELECT {cols} FROM mem_tree_jobs \
                         WHERE owner_id=$1 AND status='ready' AND available_at_ms <= $2 \
                         ORDER BY created_at_ms ASC LIMIT 1 \
                         FOR UPDATE SKIP LOCKED"
                    ),
                    &[&oid, &now_ms],
                )
                .await
            }
            None => {
                tx.query_opt(
                    &format!(
                        "SELECT {cols} FROM mem_tree_jobs \
                         WHERE status='ready' AND available_at_ms <= $1 \
                         ORDER BY created_at_ms ASC LIMIT 1 \
                         FOR UPDATE SKIP LOCKED"
                    ),
                    &[&now_ms],
                )
                .await
            }
        }
        .map_err(|e| CarrierError::Memory(e.to_string()))?;

        match row {
            Some(r) => {
                let job = Self::row_to_job(&r)?;
                tx.execute(
                    "UPDATE mem_tree_jobs SET status='running', attempts=attempts+1, \
                     locked_until_ms=$1, started_at_ms=COALESCE(started_at_ms,$2) \
                     WHERE id=$3",
                    &[&locked_until, &now_ms, &job.id],
                )
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
                tx.commit().await.map_err(|e| CarrierError::Memory(e.to_string()))?;
                Ok(Some(Job {
                    status: JobStatus::Running,
                    attempts: job.attempts + 1,
                    locked_until_ms: Some(locked_until),
                    started_at_ms: Some(now_ms),
                    ..job
                }))
            }
            None => {
                // Nothing to claim; commit the empty txn (releases the snapshot).
                tx.commit().await.map_err(|e| CarrierError::Memory(e.to_string()))?;
                Ok(None)
            }
        }
    }

    /// Mark a job as done.
    pub async fn mark_done(&self, job_id: &str) -> CarrierResult<()> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        client
            .execute(
                "UPDATE mem_tree_jobs SET status='done', completed_at_ms=$1, \
                 locked_until_ms=NULL WHERE id=$2",
                &[&now_ms, &job_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Mark a job as failed. Re-queues for retry if attempts < max_attempts,
    /// otherwise marks permanently failed.
    pub async fn mark_failed(&self, job_id: &str, error: &str) -> CarrierResult<()> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();

        let row = client
            .query_opt(
                "SELECT attempts, max_attempts FROM mem_tree_jobs WHERE id=$1",
                &[&job_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        let (attempts, max_attempts): (i32, i32) = match row {
            Some(r) => (
                r.try_get(0).map_err(|e| CarrierError::Serialization(e.to_string()))?,
                r.try_get(1).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            ),
            // Job gone - nothing to mark (source used unwrap_or(0/5) which would
            // spuriously re-queue a non-existent job; treat as no-op).
            None => return Ok(()),
        };

        if attempts >= max_attempts {
            client
                .execute(
                    "UPDATE mem_tree_jobs SET status='failed', last_error=$1, \
                     completed_at_ms=$2, locked_until_ms=NULL WHERE id=$3",
                    &[&error, &now_ms, &job_id],
                )
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        } else {
            client
                .execute(
                    "UPDATE mem_tree_jobs SET status='ready', last_error=$1, \
                     locked_until_ms=NULL, available_at_ms=$2 WHERE id=$3",
                    &[&error, &now_ms, &job_id],
                )
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
        Ok(())
    }

    /// Defer a job to be available at a future time.
    pub async fn defer(&self, job_id: &str, available_at_ms: i64) -> CarrierResult<()> {
        let client = self.client().await?;
        client
            .execute(
                "UPDATE mem_tree_jobs SET status='ready', locked_until_ms=NULL, \
                 available_at_ms=$1 WHERE id=$2",
                &[&available_at_ms, &job_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Recover stale locks - jobs that have been running for too long.
    pub async fn recover_stale_locks(&self) -> CarrierResult<usize> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let count = client
            .execute(
                "UPDATE mem_tree_jobs SET status='ready', locked_until_ms=NULL \
                 WHERE status='running' AND locked_until_ms IS NOT NULL \
                 AND locked_until_ms < $1",
                &[&now_ms],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(count as usize)
    }

    /// Count pending jobs by kind for an owner.
    pub async fn count_pending(&self, owner_id: &str, kind: Option<JobKind>) -> CarrierResult<usize> {
        let client = self.client().await?;
        let count: i64 = match kind {
            Some(k) => {
                let ks = k.as_str().to_string();
                let row = client
                    .query_one(
                        "SELECT COUNT(*) FROM mem_tree_jobs \
                         WHERE owner_id=$1 AND kind=$2 AND status IN ('ready','running')",
                        &[&owner_id, &ks],
                    )
                    .await
                    .map_err(|e| CarrierError::Memory(e.to_string()))?;
                row.get(0)
            }
            None => {
                let row = client
                    .query_one(
                        "SELECT COUNT(*) FROM mem_tree_jobs \
                         WHERE owner_id=$1 AND status IN ('ready','running')",
                        &[&owner_id],
                    )
                    .await
                    .map_err(|e| CarrierError::Memory(e.to_string()))?;
                row.get(0)
            }
        };
        Ok(count as usize)
    }

    /// Check if a source has already been ingested (dedup for document sources).
    pub async fn check_ingested(
        &self,
        owner_id: &str,
        source_kind: &str,
        source_id: &str,
    ) -> CarrierResult<bool> {
        let client = self.client().await?;
        let row = client
            .query_one(
                "SELECT COUNT(*) FROM mem_tree_ingested_sources \
                 WHERE owner_id=$1 AND source_kind=$2 AND source_id=$3",
                &[&owner_id, &source_kind, &source_id],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        let count: i64 = row.get(0);
        Ok(count > 0)
    }

    /// Mark a source as ingested.
    pub async fn mark_ingested(
        &self,
        owner_id: &str,
        source_kind: &str,
        source_id: &str,
    ) -> CarrierResult<()> {
        let client = self.client().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        client
            .execute(
                "INSERT INTO mem_tree_ingested_sources \
                    (source_kind, source_id, owner_id, ingested_at_ms) \
                 VALUES ($1,$2,$3,$4) \
                 ON CONFLICT (owner_id, source_kind, source_id) DO NOTHING",
                &[&source_kind, &source_id, &owner_id, &now_ms],
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    // -- Helpers -----------------------------------------------------------

    async fn client(&self) -> CarrierResult<deadpool_postgres::Object> {
        self.pool
            .get()
            .await
            .map_err(|e| CarrierError::Internal(format!("pg pool get: {e}")))
    }

    fn parse_kind(s: &str) -> JobKind {
        match s {
            "extract_chunk" => JobKind::ExtractChunk,
            "append_buffer" => JobKind::AppendBuffer,
            "seal" => JobKind::Seal,
            "topic_route" => JobKind::TopicRoute,
            "digest_daily" => JobKind::DigestDaily,
            "flush_stale" => JobKind::FlushStale,
            _ => JobKind::ExtractChunk,
        }
    }

    fn parse_status(s: &str) -> JobStatus {
        match s {
            "ready" => JobStatus::Ready,
            "running" => JobStatus::Running,
            "done" => JobStatus::Done,
            "failed" => JobStatus::Failed,
            "cancelled" => JobStatus::Cancelled,
            _ => JobStatus::Ready,
        }
    }

    fn row_to_job(row: &tokio_postgres::Row) -> CarrierResult<Job> {
        let kind_str: String = row
            .try_get(2)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let status_str: String = row
            .try_get(5)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        Ok(Job {
            id: row.try_get(0).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            owner_id: row.try_get(1).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            kind: Self::parse_kind(&kind_str),
            payload_json: row.try_get(3).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            dedupe_key: row.try_get(4).map_err(|e| CarrierError::Serialization(e.to_string()))?,
            status: Self::parse_status(&status_str),
            attempts: row
                .try_get::<_, i32>(6)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            max_attempts: row
                .try_get::<_, i32>(7)
                .map_err(|e| CarrierError::Serialization(e.to_string()))? as u32,
            available_at_ms: row
                .try_get(8)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            locked_until_ms: row
                .try_get(9)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            last_error: row
                .try_get(10)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            created_at_ms: row
                .try_get(11)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            started_at_ms: row
                .try_get(12)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
            completed_at_ms: row
                .try_get(13)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;

    async fn setup() -> Option<JobStore> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        Some(JobStore::new(pool))
    }

    /// Open a direct connection to the test DB (for simulating out-of-band state
    /// changes the store API doesn't expose, e.g. ageing a lock).
    async fn direct_connect() -> tokio_postgres::Client {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").unwrap();
        let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move { let _ = conn.await; });
        client
    }

    fn new_job(owner: &str, dedupe: Option<&str>) -> NewJob {
        NewJob {
            owner_id: owner.to_string(),
            kind: JobKind::Seal,
            payload_json: r#"{"tree_id":"tree_1","level":0}"#.to_string(),
            dedupe_key: dedupe.map(str::to_string),
            available_at_ms: None,
            max_attempts: None,
        }
    }

    #[tokio::test]
    async fn enqueue_and_claim() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let job_id = store.enqueue(&new_job("owner_1", Some("seal:tree_1:0"))).await.unwrap().unwrap();
        assert!(!job_id.is_empty());

        let claimed = store.claim_next(Some("owner_1")).await.unwrap().unwrap();
        assert_eq!(claimed.id, job_id);
        assert_eq!(claimed.status, JobStatus::Running);
        assert_eq!(claimed.attempts, 1);
        assert!(claimed.locked_until_ms.is_some());
    }

    #[tokio::test]
    async fn dedupe_active_suppresses() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let first = store.enqueue(&new_job("owner_1", Some("seal:tree_1:0"))).await.unwrap();
        assert!(first.is_some());
        // Same dedupe_key with an active (ready) job -> suppressed.
        let second = store.enqueue(&new_job("owner_1", Some("seal:tree_1:0"))).await.unwrap();
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn dedupe_allows_after_done() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let job_id = store.enqueue(&new_job("owner_1", Some("seal:tree_1:0"))).await.unwrap().unwrap();
        let _ = store.claim_next(Some("owner_1")).await.unwrap();
        store.mark_done(&job_id).await.unwrap();
        // Done jobs don't count as active -> re-enqueue allowed.
        let again = store.enqueue(&new_job("owner_1", Some("seal:tree_1:0"))).await.unwrap();
        assert!(again.is_some());
    }

    #[tokio::test]
    async fn null_dedupe_always_enqueues() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let a = store.enqueue(&new_job("owner_1", None)).await.unwrap();
        let b = store.enqueue(&new_job("owner_1", None)).await.unwrap();
        assert!(a.is_some() && b.is_some());
    }

    #[tokio::test]
    async fn mark_done() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let job_id = store.enqueue(&new_job("owner_1", None)).await.unwrap().unwrap();
        let _ = store.claim_next(Some("owner_1")).await.unwrap();
        store.mark_done(&job_id).await.unwrap();
        assert!(store.claim_next(Some("owner_1")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mark_failed_retry() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let mut job = new_job("owner_1", None);
        job.max_attempts = Some(3);
        let job_id = store.enqueue(&job).await.unwrap().unwrap();
        let _ = store.claim_next(Some("owner_1")).await.unwrap();
        store.mark_failed(&job_id, "timeout").await.unwrap();

        // Re-queued for retry with attempts incremented.
        let claimed = store.claim_next(Some("owner_1")).await.unwrap().unwrap();
        assert_eq!(claimed.attempts, 2);
    }

    #[tokio::test]
    async fn mark_failed_permanent_after_max() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let mut job = new_job("owner_1", None);
        job.max_attempts = Some(1);
        let job_id = store.enqueue(&job).await.unwrap().unwrap();
        let _ = store.claim_next(Some("owner_1")).await.unwrap();
        // attempts (1) >= max_attempts (1) -> permanent fail, not retried.
        store.mark_failed(&job_id, "boom").await.unwrap();
        assert!(store.claim_next(Some("owner_1")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn owner_isolation() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.enqueue(&new_job("owner_1", None)).await.unwrap();
        assert!(store.claim_next(Some("owner_2")).await.unwrap().is_none());
        // claim_next(None) crosses owners (any owner) - used by a global worker.
        assert!(store.claim_next(None).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn check_and_mark_ingested() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        assert!(!store.check_ingested("owner_1", "document", "doc_1").await.unwrap());
        store.mark_ingested("owner_1", "document", "doc_1").await.unwrap();
        assert!(store.check_ingested("owner_1", "document", "doc_1").await.unwrap());
        assert!(!store.check_ingested("owner_2", "document", "doc_1").await.unwrap());
    }

    #[tokio::test]
    async fn recover_stale_locks() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let job_id = store.enqueue(&new_job("owner_1", None)).await.unwrap().unwrap();
        let _ = store.claim_next(Some("owner_1")).await.unwrap();

        // Age the lock out-of-band (claim set locked_until = now+5min).
        let conn = direct_connect().await;
        conn.execute("UPDATE mem_tree_jobs SET locked_until_ms=1 WHERE id=$1", &[&job_id])
            .await
            .unwrap();

        let recovered = store.recover_stale_locks().await.unwrap();
        assert_eq!(recovered, 1);
        // Job is ready again -> claimable.
        let claimed = store.claim_next(Some("owner_1")).await.unwrap();
        assert!(claimed.is_some());
    }

    #[tokio::test]
    async fn concurrent_claim_distinct_rows() {
        // FOR UPDATE SKIP LOCKED: two workers claiming concurrently each get a
        // distinct job (the SQLite select-then-update could hand the same row to
        // both). We can't truly race await points, but we can verify that after
        // worker A claims job_1, worker B claims a *different* job (not job_1).
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let j1 = store.enqueue(&new_job("owner_1", None)).await.unwrap().unwrap();
        let _j2 = store.enqueue(&new_job("owner_1", None)).await.unwrap().unwrap();

        let a = store.claim_next(Some("owner_1")).await.unwrap().unwrap();
        let b = store.claim_next(Some("owner_1")).await.unwrap().unwrap();
        assert_ne!(a.id, b.id);
        // One of them is j1; both are distinct rows from the queue.
        assert!([a.id.as_str(), b.id.as_str()].contains(&j1.as_str()));
    }

    #[tokio::test]
    async fn count_pending() {
        let store = match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skip");
                return;
            }
        };
        store.enqueue(&new_job("owner_1", None)).await.unwrap();
        let mut j2 = new_job("owner_1", None);
        j2.kind = JobKind::ExtractChunk;
        store.enqueue(&j2).await.unwrap();
        assert_eq!(store.count_pending("owner_1", None).await.unwrap(), 2);
        assert_eq!(store.count_pending("owner_1", Some(JobKind::Seal)).await.unwrap(), 1);
        assert_eq!(store.count_pending("owner_1", Some(JobKind::ExtractChunk)).await.unwrap(), 1);
    }
}
