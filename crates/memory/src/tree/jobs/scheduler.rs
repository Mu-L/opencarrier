//! Daily scheduler that wakes after UTC midnight to enqueue DigestDaily
//! and FlushStale jobs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use chrono::{Datelike, TimeZone};

use crate::tree::job_store::JobStore;
use crate::tree::types::{
    DigestDailyPayload, FlushStalePayload, JobKind, NewJob,
};

/// Start the daily scheduler. Enqueues a DigestDaily for yesterday and
/// a FlushStale for today shortly after UTC midnight.
pub fn start_scheduler(
    conn: Arc<Mutex<Connection>>,
    content_root: PathBuf,
) {
    let conn1 = conn.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = enqueue_daily_jobs(&conn1) {
                tracing::warn!("[tree_jobs] scheduler enqueue failed: {e:#}");
            }
            let sleep = next_sleep_duration();
            tokio::time::sleep(sleep).await;
        }
    });

    // Also start a periodic stale-lock recovery
    let conn2 = conn.clone();
    tokio::spawn(async move {
        loop {
            let job_store = JobStore::new(conn2.clone());
            if let Err(e) = job_store.recover_stale_locks() {
                tracing::warn!("[tree_jobs] stale lock recovery failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

    let _ = content_root; // held for future use if needed
}

fn enqueue_daily_jobs(conn: &Arc<Mutex<Connection>>) -> types::error::CarrierResult<()> {
    let job_store = JobStore::new(conn.clone());
    let now = chrono::Utc::now();
    let yesterday = now.date_naive() - chrono::Duration::days(1);
    let date_iso = yesterday.format("%Y-%m-%d").to_string();

    // Find all owners that have trees
    let owners = list_owners_with_trees(conn)?;

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
                .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job)?;

        // FlushStale for today
        let flush_payload = FlushStalePayload::default();
        let today_iso = now.date_naive().format("%Y-%m-%d").to_string();
        let dedupe_key = format!("flush_stale:{}:{}", owner_id, today_iso);
        let new_job = NewJob {
            owner_id: owner_id.clone(),
            kind: JobKind::FlushStale,
            payload_json: serde_json::to_string(&flush_payload)
                .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job)?;
    }

    if !owners.is_empty() {
        tracing::info!(
            "[tree_jobs] scheduler enqueued daily jobs for {} owners",
            owners.len()
        );
    }

    Ok(())
}

/// Manually trigger a digest for a specific owner and date.
pub fn trigger_digest(
    conn: &Arc<Mutex<Connection>>,
    owner_id: &str,
    date: chrono::NaiveDate,
) -> types::error::CarrierResult<Option<String>> {
    let job_store = JobStore::new(conn.clone());
    let date_iso = date.format("%Y-%m-%d").to_string();

    let payload = DigestDailyPayload {
        date_iso: date_iso.clone(),
    };
    let dedupe_key = format!("digest_daily:{}:{}", owner_id, date_iso);
    let new_job = NewJob {
        owner_id: owner_id.to_string(),
        kind: JobKind::DigestDaily,
        payload_json: serde_json::to_string(&payload)
            .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
        dedupe_key: Some(dedupe_key),
        available_at_ms: None,
        max_attempts: None,
    };

    let job_id = job_store.enqueue(&new_job)?;
    if job_id.is_some() {
        tracing::info!(
            "[tree_jobs] manual digest trigger enqueued owner={} date={}",
            owner_id,
            date_iso
        );
    }
    Ok(job_id)
}

fn list_owners_with_trees(
    conn: &Arc<Mutex<Connection>>,
) -> types::error::CarrierResult<Vec<String>> {
    let c = conn
        .lock()
        .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?;

    let mut stmt = c
        .prepare("SELECT DISTINCT owner_id FROM mem_tree_trees")
        .map_err(|e| types::error::CarrierError::Memory(e.to_string()))?;

    let rows = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| types::error::CarrierError::Memory(e.to_string()))?;

    let mut owners = Vec::new();
    for row in rows {
        owners.push(row.map_err(|e| types::error::CarrierError::Memory(e.to_string()))?);
    }
    Ok(owners)
}

fn next_sleep_duration() -> Duration {
    let now = chrono::Utc::now();
    let tomorrow = now.date_naive() + chrono::Duration::days(1);
    let next = chrono::Utc
        .with_ymd_and_hms(tomorrow.year(), tomorrow.month(), tomorrow.day(), 0, 5, 0)
        .single()
        .unwrap_or_else(|| now + chrono::Duration::hours(24));
    (next - now)
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(60))
}
