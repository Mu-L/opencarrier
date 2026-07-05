//! Persistent cron job store backed by SQLite.
//!
//! Replaces the previous `cron_jobs.json` file. The CronScheduler still uses
//! an in-memory DashMap for hot-path operations; this store handles
//! persistence (load/save/delete) against the central `opencarrier.db`.

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use rusqlite::Connection;
use types::error::{CarrierError, CarrierResult};
use types::scheduler::{CronAction, CronDelivery, CronJob, CronJobId, CronSchedule};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Runtime metadata not stored in CronJob itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMeta {
    pub job: CronJob,
    pub one_shot: bool,
    pub last_status: Option<String>,
    pub consecutive_errors: u32,
}

impl JobMeta {
    pub fn new(job: CronJob, one_shot: bool) -> Self {
        Self { job, one_shot, last_status: None, consecutive_errors: 0 }
    }
}

/// SQLite-backed cron job store.
#[derive(Clone)]
pub struct CronJobStore {
    conn: Arc<Mutex<Connection>>,
}

impl CronJobStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Load all persisted jobs from the database.
    pub fn load_all(&self) -> CarrierResult<Vec<JobMeta>> {
        let conn = self.conn.lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, owner_id, sender_id, name, enabled, schedule, action, delivery, \
                    one_shot, last_status, consecutive_errors, created_at, last_run, next_run \
             FROM cron_jobs"
        ).map_err(|e| CarrierError::Memory(e.to_string()))?;
        let rows = stmt.query_map([], |row| {
            Ok(RowData {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                owner_id: row.get(2)?,
                sender_id: row.get(3)?,
                name: row.get(4)?,
                enabled: row.get(5)?,
                schedule_json: row.get(6)?,
                action_json: row.get(7)?,
                delivery_json: row.get(8)?,
                one_shot: row.get(9)?,
                last_status: row.get(10)?,
                consecutive_errors: row.get(11)?,
                created_at: row.get(12)?,
                last_run: row.get(13)?,
                next_run: row.get(14)?,
            })
        }).map_err(|e| CarrierError::Memory(e.to_string()))?;

        let mut metas = Vec::new();
        for row in rows {
            let r = row.map_err(|e| CarrierError::Memory(e.to_string()))?;
            if let Some(meta) = row_to_meta(r) {
                metas.push(meta);
            }
        }
        Ok(metas)
    }

    /// Persist all jobs (replaces entire table contents).
    pub fn save_all(&self, metas: &[JobMeta]) -> CarrierResult<()> {
        let conn = self.conn.lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        conn.execute("DELETE FROM cron_jobs", [])
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        for meta in metas {
            self.insert_meta(&conn, meta)?;
        }
        Ok(())
    }

    /// Insert or update a single job.
    pub fn upsert(&self, meta: &JobMeta) -> CarrierResult<()> {
        let conn = self.conn.lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        self.insert_meta(&conn, meta)
    }

    /// Delete a job by ID.
    pub fn delete(&self, id: &str) -> CarrierResult<()> {
        let conn = self.conn.lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        conn.execute("DELETE FROM cron_jobs WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    fn insert_meta(&self, conn: &Connection, meta: &JobMeta) -> CarrierResult<()> {
        let schedule_json = serde_json::to_string(&meta.job.schedule)
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let action_json = serde_json::to_string(&meta.job.action)
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let delivery_json = serde_json::to_string(&meta.job.delivery)
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let created_at = meta.job.created_at.to_rfc3339();
        let last_run = meta.job.last_run.map(|t| t.to_rfc3339());
        let next_run = meta.job.next_run.map(|t| t.to_rfc3339());
        let agent_id = meta.job.agent_id.to_string();
        let id = meta.job.id.to_string();

        conn.execute(
            "INSERT INTO cron_jobs (id, agent_id, owner_id, sender_id, name, enabled, schedule, action, delivery, \
                                    one_shot, last_status, consecutive_errors, created_at, last_run, next_run) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
             ON CONFLICT(id) DO UPDATE SET \
               agent_id=?2, owner_id=?3, sender_id=?4, name=?5, enabled=?6, schedule=?7, action=?8, \
               delivery=?9, one_shot=?10, last_status=?11, consecutive_errors=?12, \
               last_run=?14, next_run=?15",
            rusqlite::params![
                id, agent_id, meta.job.owner_id, meta.job.sender_id, meta.job.name,
                meta.job.enabled as i32, schedule_json, action_json, delivery_json,
                meta.one_shot as i32, meta.last_status, meta.consecutive_errors as i32,
                created_at, last_run, next_run,
            ],
        ).map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }
}

struct RowData {
    id: String,
    agent_id: String,
    owner_id: Option<String>,
    sender_id: Option<String>,
    name: String,
    enabled: i32,
    schedule_json: String,
    action_json: String,
    delivery_json: String,
    one_shot: i32,
    last_status: Option<String>,
    consecutive_errors: i32,
    created_at: String,
    last_run: Option<String>,
    next_run: Option<String>,
}

fn row_to_meta(r: RowData) -> Option<JobMeta> {
    let schedule: CronSchedule = serde_json::from_str(&r.schedule_json).ok()?;
    let action: CronAction = serde_json::from_str(&r.action_json).ok()?;
    let delivery: CronDelivery = serde_json::from_str(&r.delivery_json).ok()?;
    let created_at: DateTime<Utc> = r.created_at.parse().ok()?;
    let last_run = r.last_run.and_then(|s| s.parse::<DateTime<Utc>>().ok());
    let next_run = r.next_run.and_then(|s| s.parse::<DateTime<Utc>>().ok());
    let id = CronJobId::from_str(&r.id).ok()?;
    let agent_id = types::agent::AgentId::from_string(&r.agent_id);

    let job = CronJob {
        id,
        agent_id,
        owner_id: r.owner_id,
        sender_id: r.sender_id,
        name: r.name,
        enabled: r.enabled != 0,
        schedule,
        action,
        delivery,
        created_at,
        last_run,
        next_run,
    };

    Some(JobMeta {
        job,
        one_shot: r.one_shot != 0,
        last_status: r.last_status,
        consecutive_errors: r.consecutive_errors as u32,
    })
}
