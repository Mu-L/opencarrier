//! Daily scheduler that wakes after Asia/Shanghai midnight (see
//! [`crate::digest::DIGEST_TZ`]) to enqueue DigestDaily and FlushStale jobs.
//!
//! Ported from `memory::tree::jobs::scheduler` - same schedule, backed by the
//! PG async pool. `list_owners_with_trees` is rewritten as a PG query
//! (`SELECT DISTINCT owner_id FROM mem_tree_trees`).

use std::path::PathBuf;
use std::time::Duration;

use chrono::{Datelike, TimeZone};
use deadpool_postgres::Pool;
use types::error::{CarrierError, CarrierResult};

use crate::pg::job_store::JobStore;
use memory::tree::types::{DigestDailyPayload, FlushStalePayload, JobKind, NewJob};

/// Start the daily scheduler. Enqueues a DigestDaily for yesterday and a
/// FlushStale for today shortly after local (Asia/Shanghai) midnight, for
/// every owner that has trees. Also runs a periodic stale-lock recovery.
pub fn start_scheduler(pool: Pool, content_root: PathBuf) {
    let pool1 = pool.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = enqueue_daily_jobs(&pool1).await {
                tracing::warn!("[tree_jobs] scheduler enqueue failed: {e:#}");
            }
            let sleep = next_sleep_duration();
            tokio::time::sleep(sleep).await;
        }
    });

    // Periodic stale-lock recovery.
    let pool2 = pool.clone();
    tokio::spawn(async move {
        loop {
            let job_store = JobStore::new(pool2.clone());
            if let Err(e) = job_store.recover_stale_locks().await {
                tracing::warn!("[tree_jobs] stale lock recovery failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

    let _ = content_root; // held for future use if needed
    let _ = pool;
}

async fn enqueue_daily_jobs(pool: &Pool) -> CarrierResult<()> {
    let job_store = JobStore::new(pool.clone());
    // Digest days run on the Asia/Shanghai calendar ([`crate::digest::digest_tz`])
    // — a user's "day" ends at local midnight, not 08:00 UTC.
    let tz = crate::digest::digest_tz();
    let now_local = tz.from_utc_datetime(&chrono::Utc::now().naive_utc());
    let yesterday = now_local.date_naive() - chrono::Duration::days(1);
    let date_iso = yesterday.format("%Y-%m-%d").to_string();

    // Find all owners that have trees.
    let owners = list_owners_with_trees(pool).await?;

    for owner_id in &owners {
        // DigestDaily for yesterday
        let digest_payload = DigestDailyPayload {
            date_iso: date_iso.clone(),
        };
        let dedupe_key = format!("digest_daily:{}:{}", owner_id, date_iso);
        let new_job = NewJob {
            owner_id: owner_id.clone(),
            kind: JobKind::DigestDaily,
            payload_json: serde_json::to_string(&digest_payload)
                .map_err(|e| CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job).await?;

        // FlushStale for today
        let flush_payload = FlushStalePayload::default();
        let today_iso = now_local.date_naive().format("%Y-%m-%d").to_string();
        let dedupe_key = format!("flush_stale:{}:{}", owner_id, today_iso);
        let new_job = NewJob {
            owner_id: owner_id.clone(),
            kind: JobKind::FlushStale,
            payload_json: serde_json::to_string(&flush_payload)
                .map_err(|e| CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job).await?;
    }

    if !owners.is_empty() {
        tracing::info!(
            "[tree_jobs] scheduler enqueued daily jobs for {} owners",
            owners.len()
        );
    }

    Ok(())
}

async fn list_owners_with_trees(pool: &Pool) -> CarrierResult<Vec<String>> {
    let client = pool
        .get()
        .await
        .map_err(|e| CarrierError::Internal(format!("pg pool get: {e}")))?;
    let rows = client
        .query("SELECT DISTINCT owner_id FROM mem_tree_trees", &[])
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    rows.iter()
        .map(|r| {
            r.try_get(0)
                .map_err(|e| CarrierError::Serialization(e.to_string()))
        })
        .collect::<CarrierResult<Vec<String>>>()
}

fn next_sleep_duration() -> Duration {
    // Wake at 00:05 Asia/Shanghai (16:05 UTC the previous day) so the digest
    // for "yesterday" is enqueued right after the local day rolls over.
    let tz = crate::digest::digest_tz();
    let now_local = tz.from_utc_datetime(&chrono::Utc::now().naive_utc());
    let tomorrow = now_local.date_naive() + chrono::Duration::days(1);
    let next_local = tz
        .with_ymd_and_hms(tomorrow.year(), tomorrow.month(), tomorrow.day(), 0, 5, 0)
        .single()
        .unwrap_or_else(|| now_local + chrono::Duration::hours(24));
    (next_local - now_local)
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(60))
}
