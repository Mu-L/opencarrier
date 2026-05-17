//! MemorySubstrate: unified memory substrate built around the tree memory system.
//!
//! Composes the system KV store, session store, invite store, cron delivery store,
//! and tree memory behind a single API.

use crate::cron_delivery::CronDeliveryStore;
use crate::invites::InviteStore;
use crate::migration::run_migrations;
use crate::session::{Session, SessionStore};
use crate::system_kv::SystemKV;
use crate::tree::ingest::IngestPipeline;
use crate::tree::retrieval;
use crate::tree::types::SourceKind;
use crate::usage::UsageStore;

use types::agent::{AgentEntry, AgentId, SessionId};
use types::error::{CarrierError, CarrierResult};
use types::memory_tree::{
    EntityMatch, IngestRequest, IngestResult, QueryResponse, TreeSummary,
    DrillDownQuery, EntitySearch, FetchLeavesQuery, GlobalQuery, SourceQuery, TopicQuery,
};

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The unified memory substrate. Tree memory is the primary memory interface;
/// system_kv, sessions, invites, and cron_delivery are infrastructure stores.
pub struct MemorySubstrate {
    conn: Arc<Mutex<Connection>>,
    system_kv: SystemKV,
    sessions: SessionStore,
    invites: InviteStore,
    cron_delivery: CronDeliveryStore,
    content_root: PathBuf,
}

impl MemorySubstrate {
    /// Open or create a memory substrate at the given database path.
    pub fn open(db_path: &Path) -> CarrierResult<Self> {
        let conn = Connection::open(db_path).map_err(|e| CarrierError::Memory(e.to_string()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
        )
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        run_migrations(&conn).map_err(|e| CarrierError::Memory(e.to_string()))?;
        let shared = Arc::new(Mutex::new(conn));

        // Default content root: sibling to the db file
        let content_root = db_path.parent()
            .unwrap_or(Path::new("."))
            .join("memory_tree")
            .join("content");

        Ok(Self {
            conn: Arc::clone(&shared),
            system_kv: SystemKV::new(Arc::clone(&shared)),
            sessions: SessionStore::new(Arc::clone(&shared)),
            invites: InviteStore::new(Arc::clone(&shared)),
            cron_delivery: CronDeliveryStore::new(Arc::clone(&shared)),
            content_root,
        })
    }

    /// Create an in-memory substrate (for testing).
    pub fn open_in_memory() -> CarrierResult<Self> {
        let conn = Connection::open_in_memory().map_err(|e| CarrierError::Memory(e.to_string()))?;
        run_migrations(&conn).map_err(|e| CarrierError::Memory(e.to_string()))?;
        let shared = Arc::new(Mutex::new(conn));

        Ok(Self {
            conn: Arc::clone(&shared),
            system_kv: SystemKV::new(Arc::clone(&shared)),
            sessions: SessionStore::new(Arc::clone(&shared)),
            invites: InviteStore::new(Arc::clone(&shared)),
            cron_delivery: CronDeliveryStore::new(Arc::clone(&shared)),
            content_root: PathBuf::from("/tmp/opencarrier_tree_content"),
        })
    }

    /// Get a reference to the cron delivery store (last-channel tracking + buffer).
    pub fn cron_delivery(&self) -> &CronDeliveryStore {
        &self.cron_delivery
    }

    /// Get a reference to the invite store.
    pub fn invites(&self) -> &InviteStore {
        &self.invites
    }

    /// Create a new UsageStore from the shared connection.
    pub fn usage(&self) -> UsageStore {
        UsageStore::new(Arc::clone(&self.conn))
    }

    /// Get the shared database connection (for constructing stores from outside).
    pub fn usage_conn(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    // -----------------------------------------------------------------
    // System KV operations (agent entries, schedules, config)
    // -----------------------------------------------------------------

    /// Save an agent entry to persistent storage.
    pub fn save_agent(&self, entry: &AgentEntry) -> CarrierResult<()> {
        self.system_kv.save_agent(entry)
    }

    /// Load an agent entry from persistent storage.
    pub fn load_agent(&self, agent_id: AgentId) -> CarrierResult<Option<AgentEntry>> {
        self.system_kv.load_agent(agent_id)
    }

    /// Remove an agent from persistent storage and cascade-delete sessions.
    pub fn remove_agent(&self, agent_id: AgentId) -> CarrierResult<()> {
        let _ = self.sessions.delete_agent_sessions(agent_id);
        self.system_kv.remove_agent(agent_id)
    }

    /// Load all agent entries from persistent storage.
    pub fn load_all_agents(&self) -> CarrierResult<Vec<AgentEntry>> {
        self.system_kv.load_all_agents()
    }

    /// List all saved agents.
    pub fn list_agents(&self) -> CarrierResult<Vec<(String, String, String)>> {
        self.system_kv.list_agents()
    }

    /// Synchronous get from the system KV store.
    pub fn system_kv_get(
        &self,
        agent_id: AgentId,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<Option<serde_json::Value>> {
        self.system_kv.get(agent_id, owner_id, user_id, key)
    }

    /// Synchronous set in the system KV store.
    pub fn system_kv_set(
        &self,
        agent_id: AgentId,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> CarrierResult<()> {
        self.system_kv.set(agent_id, owner_id, user_id, key, value)
    }

    /// List all KV pairs for an agent (per-user).
    pub fn list_kv(
        &self,
        agent_id: AgentId,
        owner_id: &str,
        user_id: &str,
    ) -> CarrierResult<Vec<(String, serde_json::Value)>> {
        self.system_kv.list_kv(agent_id, owner_id, user_id)
    }

    /// Delete a KV entry for an agent (per-user).
    pub fn system_kv_delete(
        &self,
        agent_id: AgentId,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<()> {
        self.system_kv.delete(agent_id, owner_id, user_id, key)
    }

    // -----------------------------------------------------------------
    // Session operations
    // -----------------------------------------------------------------

    /// Get a session by ID.
    pub fn get_session(&self, session_id: SessionId) -> CarrierResult<Option<Session>> {
        self.sessions.get_session(session_id)
    }

    /// Save a session.
    pub fn save_session(&self, session: &Session) -> CarrierResult<()> {
        self.sessions.save_session(session)
    }

    /// Save a session asynchronously — runs the SQLite write in a blocking
    /// thread so the tokio runtime stays responsive.
    pub async fn save_session_async(&self, session: &Session) -> CarrierResult<()> {
        let sessions = self.sessions.clone();
        let session = session.clone();
        tokio::task::spawn_blocking(move || sessions.save_session(&session))
            .await
            .map_err(|e| CarrierError::Internal(e.to_string()))?
    }

    /// Create a new empty session for an agent.
    pub fn create_session(&self, agent_id: AgentId) -> CarrierResult<Session> {
        self.sessions.create_session(agent_id)
    }

    /// List all sessions with metadata.
    pub fn list_sessions(&self) -> CarrierResult<Vec<serde_json::Value>> {
        self.sessions.list_sessions()
    }

    /// Delete a session by ID.
    pub fn delete_session(&self, session_id: SessionId) -> CarrierResult<()> {
        self.sessions.delete_session(session_id)
    }

    /// Delete all sessions belonging to an agent.
    pub fn delete_agent_sessions(&self, agent_id: AgentId) -> CarrierResult<()> {
        self.sessions.delete_agent_sessions(agent_id)
    }

    /// Set or clear a session label.
    pub fn set_session_label(
        &self,
        session_id: SessionId,
        label: Option<&str>,
    ) -> CarrierResult<()> {
        self.sessions.set_session_label(session_id, label)
    }

    /// Find a session by label for a given agent.
    pub fn find_session_by_label(
        &self,
        agent_id: AgentId,
        label: &str,
    ) -> CarrierResult<Option<Session>> {
        self.sessions.find_session_by_label(agent_id, label)
    }

    /// List all sessions for a specific agent.
    pub fn list_agent_sessions(&self, agent_id: AgentId) -> CarrierResult<Vec<serde_json::Value>> {
        self.sessions.list_agent_sessions(agent_id)
    }

    /// Create a new session with an optional label.
    pub fn create_session_with_label(
        &self,
        agent_id: AgentId,
        label: Option<&str>,
    ) -> CarrierResult<Session> {
        self.sessions.create_session_with_label(agent_id, label)
    }

    /// Write a human-readable JSONL mirror of a session to disk.
    pub fn write_jsonl_mirror(
        &self,
        session: &Session,
        sessions_dir: &Path,
        owner_id: Option<&str>,
        sender_id: Option<&str>,
        home_dir: Option<&Path>,
        agent_name: Option<&str>,
    ) -> Result<(), std::io::Error> {
        self.sessions
            .write_jsonl_mirror(session, sessions_dir, owner_id, sender_id, home_dir, agent_name)
    }

    // -----------------------------------------------------------------
    // Tree memory operations
    // -----------------------------------------------------------------

    /// Ingest messages into the tree memory system.
    pub fn tree_ingest(&self, req: &IngestRequest) -> CarrierResult<IngestResult> {
        let pipeline = IngestPipeline::new(Arc::clone(&self.conn), self.content_root.clone());
        pipeline.ingest(req)
    }

    /// Async wrapper for tree_ingest (runs in blocking thread).
    pub async fn tree_ingest_async(&self, req: IngestRequest) -> CarrierResult<IngestResult> {
        let conn = Arc::clone(&self.conn);
        let content_root = self.content_root.clone();
        tokio::task::spawn_blocking(move || {
            let pipeline = IngestPipeline::new(conn, content_root);
            pipeline.ingest(&req)
        })
        .await
        .map_err(|e| CarrierError::Internal(e.to_string()))?
    }

    /// Query source tree summaries.
    pub fn tree_query_source(&self, req: &SourceQuery<'_>) -> CarrierResult<QueryResponse> {
        let source_kind = req.source_kind.and_then(|k| match k {
            "chat" => Some(SourceKind::Chat),
            "email" => Some(SourceKind::Email),
            "document" => Some(SourceKind::Document),
            _ => None,
        });
        retrieval::source::query_source(
            &self.conn,
            req.owner_id,
            req.source_id,
            source_kind,
            req.time_window_days,
            req.limit,
        )
    }

    /// Query global tree summaries.
    pub fn tree_query_global(&self, req: &GlobalQuery<'_>) -> CarrierResult<QueryResponse> {
        retrieval::global::query_global(
            &self.conn,
            req.owner_id,
            req.time_window_days,
            req.limit,
        )
    }

    /// Query topic tree by entity.
    pub fn tree_query_topic(&self, req: &TopicQuery<'_>) -> CarrierResult<QueryResponse> {
        retrieval::topic::query_topic(
            &self.conn,
            req.owner_id,
            req.entity_id,
            req.time_window_days,
            req.limit,
        )
    }

    /// Search entities by substring.
    pub fn tree_search_entities(&self, req: &EntitySearch<'_>) -> CarrierResult<Vec<EntityMatch>> {
        let kind = req.kind.map(crate::tree::entity_store::EntityStore::parse_entity_kind);
        retrieval::search::search_entities(
            &self.conn,
            req.owner_id,
            req.query,
            kind,
            req.limit,
        )
    }

    /// Drill down from a summary node to its children.
    pub fn tree_drill_down(&self, req: &DrillDownQuery<'_>) -> CarrierResult<QueryResponse> {
        let hits = retrieval::drill_down::drill_down(
            &self.conn,
            req.owner_id,
            req.node_id,
            2, // default depth
            Some(req.limit),
        )?;
        let total = hits.len();
        let truncated = total > req.limit;
        Ok(QueryResponse {
            hits,
            total,
            truncated,
        })
    }

    /// Fetch all leaf chunks under a summary node.
    pub fn tree_fetch_leaves(&self, req: &FetchLeavesQuery<'_>) -> CarrierResult<QueryResponse> {
        retrieval::fetch::fetch_leaves(
            &self.conn,
            req.owner_id,
            req.node_id,
            req.limit,
        )
    }

    /// List all source trees for an owner.
    pub fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> CarrierResult<Vec<TreeSummary>> {
        use types::memory_tree::TreeKind;
        let tree_store = crate::tree::tree_store::TreeTreeStore::new(Arc::clone(&self.conn));
        let mut trees = tree_store.list_trees(owner_id, Some(TreeKind::Source), limit)?;
        if let Some(sk) = source_kind {
            trees.retain(|t| t.scope.starts_with(&format!("{sk}:")));
        }
        Ok(trees)
    }

    // -----------------------------------------------------------------
    // Task queue operations
    // -----------------------------------------------------------------

    /// Post a new task to the shared queue. Returns the task ID.
    pub async fn task_post(
        &self,
        title: &str,
        description: &str,
        assigned_to: Option<&str>,
        created_by: Option<&str>,
    ) -> CarrierResult<String> {
        let conn = Arc::clone(&self.conn);
        let title = title.to_string();
        let description = description.to_string();
        let assigned_to = assigned_to.unwrap_or("").to_string();
        let created_by = created_by.unwrap_or("").to_string();

        tokio::task::spawn_blocking(move || {
            let id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now().to_rfc3339();
            let db = conn.lock().map_err(|e| CarrierError::Internal(e.to_string()))?;
            db.execute(
                "INSERT INTO task_queue (id, agent_id, task_type, payload, status, priority, created_at, title, description, assigned_to, created_by)
                 VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![id, &created_by, &title, b"", now, title, description, assigned_to, created_by],
            )
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
            Ok(id)
        })
        .await
        .map_err(|e| CarrierError::Internal(e.to_string()))?
    }

    /// Claim the next pending task. Returns task JSON or None.
    pub async fn task_claim(&self, agent_id: &str) -> CarrierResult<Option<serde_json::Value>> {
        let conn = Arc::clone(&self.conn);
        let agent_id = agent_id.to_string();

        tokio::task::spawn_blocking(move || {
            let db = conn.lock().map_err(|e| CarrierError::Internal(e.to_string()))?;

            let sql = "SELECT id, title, description, assigned_to, created_by, created_at
                     FROM task_queue
                     WHERE status = 'pending' AND (assigned_to = ?1 OR assigned_to = '')
                     ORDER BY priority DESC, created_at ASC
                     LIMIT 1";

            let mut stmt = db.prepare(sql).map_err(|e| CarrierError::Memory(e.to_string()))?;

            let result = stmt.query_row(rusqlite::params![agent_id.clone()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            });

            match result {
                Ok((id, title, description, assigned, created_by, created_at)) => {
                    db.execute(
                        "UPDATE task_queue SET status = 'in_progress', assigned_to = ?2 WHERE id = ?1",
                        rusqlite::params![id, agent_id],
                    ).map_err(|e| CarrierError::Memory(e.to_string()))?;

                    Ok(Some(serde_json::json!({
                        "id": id,
                        "title": title,
                        "description": description,
                        "status": "in_progress",
                        "assigned_to": if assigned.is_empty() { &agent_id } else { &assigned },
                        "created_by": created_by,
                        "created_at": created_at,
                    })))
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(CarrierError::Memory(e.to_string())),
            }
        })
        .await
        .map_err(|e| CarrierError::Internal(e.to_string()))?
    }

    /// Mark a task as completed with a result string.
    pub async fn task_complete(&self, task_id: &str, result: &str) -> CarrierResult<()> {
        let conn = Arc::clone(&self.conn);
        let task_id = task_id.to_string();
        let result = result.to_string();

        tokio::task::spawn_blocking(move || {
            let now = chrono::Utc::now().to_rfc3339();
            let db = conn.lock().map_err(|e| CarrierError::Internal(e.to_string()))?;
            let rows = db.execute(
                "UPDATE task_queue SET status = 'completed', result = ?2, completed_at = ?3 WHERE id = ?1",
                rusqlite::params![task_id, result, now],
            ).map_err(|e| CarrierError::Memory(e.to_string()))?;
            if rows == 0 {
                return Err(CarrierError::Internal(format!("Task not found: {task_id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| CarrierError::Internal(e.to_string()))?
    }

    /// List tasks, optionally filtered by status.
    pub async fn task_list(&self, status: Option<&str>) -> CarrierResult<Vec<serde_json::Value>> {
        let conn = Arc::clone(&self.conn);
        let status = status.map(|s| s.to_string());

        tokio::task::spawn_blocking(move || {
            let db = conn.lock().map_err(|e| CarrierError::Internal(e.to_string()))?;
            let (sql, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match &status {
                Some(s) => (
                    "SELECT id, title, description, status, assigned_to, created_by, created_at, completed_at, result FROM task_queue WHERE status = ?1 ORDER BY created_at DESC",
                    vec![Box::new(s.clone())],
                ),
                None => (
                    "SELECT id, title, description, status, assigned_to, created_by, created_at, completed_at, result FROM task_queue ORDER BY created_at DESC",
                    vec![],
                ),
            };

            let mut stmt = db.prepare(sql).map_err(|e| CarrierError::Memory(e.to_string()))?;
            let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "title": row.get::<_, String>(1).unwrap_or_default(),
                    "description": row.get::<_, String>(2).unwrap_or_default(),
                    "status": row.get::<_, String>(3)?,
                    "assigned_to": row.get::<_, String>(4).unwrap_or_default(),
                    "created_by": row.get::<_, String>(5).unwrap_or_default(),
                    "created_at": row.get::<_, String>(6).unwrap_or_default(),
                    "completed_at": row.get::<_, Option<String>>(7).unwrap_or(None),
                    "result": row.get::<_, Option<String>>(8).unwrap_or(None),
                }))
            }).map_err(|e| CarrierError::Memory(e.to_string()))?;

            let mut tasks = Vec::new();
            for row in rows {
                tasks.push(row.map_err(|e| CarrierError::Memory(e.to_string()))?);
            }
            Ok(tasks)
        })
        .await
        .map_err(|e| CarrierError::Internal(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_kv_set_get() {
        let substrate = MemorySubstrate::open_in_memory().unwrap();
        let agent_id = AgentId::new();
        substrate
            .system_kv_set(
                agent_id,
                "user1",
                "user1",
                "test_key",
                serde_json::json!("test_value"),
            )
            .unwrap();
        let val = substrate
            .system_kv_get(agent_id, "user1", "user1", "test_key")
            .unwrap();
        assert_eq!(val, Some(serde_json::json!("test_value")));
    }

    #[tokio::test]
    async fn test_task_post_and_list() {
        let substrate = MemorySubstrate::open_in_memory().unwrap();
        let id = substrate
            .task_post(
                "Review code",
                "Check the auth module for issues",
                Some("auditor"),
                Some("orchestrator"),
            )
            .await
            .unwrap();
        assert!(!id.is_empty());

        let tasks = substrate.task_list(Some("pending")).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["title"], "Review code");
        assert_eq!(tasks[0]["assigned_to"], "auditor");
        assert_eq!(tasks[0]["status"], "pending");
    }

    #[tokio::test]
    async fn test_task_claim_and_complete() {
        let substrate = MemorySubstrate::open_in_memory().unwrap();
        let task_id = substrate
            .task_post(
                "Audit endpoint",
                "Security audit the /api/login endpoint",
                Some("auditor"),
                None,
            )
            .await
            .unwrap();

        let claimed = substrate.task_claim("auditor").await.unwrap();
        assert!(claimed.is_some());
        let claimed = claimed.unwrap();
        assert_eq!(claimed["id"], task_id);
        assert_eq!(claimed["status"], "in_progress");

        substrate
            .task_complete(&task_id, "No vulnerabilities found")
            .await
            .unwrap();

        let tasks = substrate.task_list(Some("completed")).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["result"], "No vulnerabilities found");
    }

    #[tokio::test]
    async fn test_task_claim_empty() {
        let substrate = MemorySubstrate::open_in_memory().unwrap();
        let claimed = substrate.task_claim("nobody").await.unwrap();
        assert!(claimed.is_none());
    }
}
