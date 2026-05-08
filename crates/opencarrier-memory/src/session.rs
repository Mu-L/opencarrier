//! Session management — load/save conversation history.

use chrono::Utc;
use opencarrier_types::agent::{AgentId, SessionId};
use opencarrier_types::error::{OpenCarrierError, OpenCarrierResult};
use opencarrier_types::message::{ContentBlock, Message, MessageContent, Role};
use rusqlite::Connection;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A conversation session with message history.
#[derive(Debug, Clone)]
pub struct Session {
    /// Session ID.
    pub id: SessionId,
    /// Owning agent ID.
    pub agent_id: AgentId,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Estimated token count for the context window.
    pub context_window_tokens: u64,
    /// Optional human-readable session label.
    pub label: Option<String>,
}

/// Session store backed by SQLite.
#[derive(Clone)]
pub struct SessionStore {
    conn: Arc<Mutex<Connection>>,
}

impl SessionStore {
    /// Create a new session store wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Load a session from the database.
    pub fn get_session(&self, session_id: SessionId) -> OpenCarrierResult<Option<Session>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT agent_id, messages, context_window_tokens, label FROM sessions WHERE id = ?1")
            .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![session_id.0.to_string()], |row| {
            let agent_str: String = row.get(0)?;
            let messages_blob: Vec<u8> = row.get(1)?;
            let tokens: i64 = row.get(2)?;
            let label: Option<String> = row.get(3).unwrap_or(None);
            Ok((agent_str, messages_blob, tokens, label))
        });

        match result {
            Ok((agent_str, messages_blob, tokens, label)) => {
                let agent_id = uuid::Uuid::parse_str(&agent_str)
                    .map(AgentId)
                    .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;
                let messages: Vec<Message> = rmp_serde::from_slice(&messages_blob)
                    .map_err(|e| OpenCarrierError::Serialization(e.to_string()))?;
                Ok(Some(Session {
                    id: session_id,
                    agent_id,
                    messages,
                    context_window_tokens: tokens as u64,
                    label,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(OpenCarrierError::Memory(e.to_string())),
        }
    }

    /// Save a session to the database.
    pub fn save_session(&self, session: &Session) -> OpenCarrierResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;
        let messages_blob = rmp_serde::to_vec_named(&session.messages)
            .map_err(|e| OpenCarrierError::Serialization(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sessions (id, agent_id, messages, context_window_tokens, label, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET messages = ?3, context_window_tokens = ?4, label = ?5, updated_at = ?6",
            rusqlite::params![
                session.id.0.to_string(),
                session.agent_id.0.to_string(),
                messages_blob,
                session.context_window_tokens as i64,
                session.label.as_deref(),
                now,
            ],
        )
        .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Delete a session from the database.
    pub fn delete_session(&self, session_id: SessionId) -> OpenCarrierResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;
        conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            rusqlite::params![session_id.0.to_string()],
        )
        .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Delete all sessions belonging to an agent.
    pub fn delete_agent_sessions(&self, agent_id: AgentId) -> OpenCarrierResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;
        conn.execute(
            "DELETE FROM sessions WHERE agent_id = ?1",
            rusqlite::params![agent_id.0.to_string()],
        )
        .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// List all sessions with metadata (session_id, agent_id, message_count, created_at).
    pub fn list_sessions(&self) -> OpenCarrierResult<Vec<serde_json::Value>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;

        let sql = "SELECT id, agent_id, messages, created_at, label FROM sessions ORDER BY created_at DESC";
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;

        let row_data: Vec<rusqlite::Result<serde_json::Value>> = stmt
            .query_map([], Self::session_row_to_json)
            .map_err(|e| OpenCarrierError::Memory(e.to_string()))?
            .collect();

        let mut sessions = Vec::new();
        for row in row_data {
            sessions.push(row.map_err(|e| OpenCarrierError::Memory(e.to_string()))?);
        }
        Ok(sessions)
    }

    /// Helper to map a session row to JSON.
    fn session_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
        let session_id: String = row.get(0)?;
        let agent_id: String = row.get(1)?;
        let messages_blob: Vec<u8> = row.get(2)?;
        let created_at: String = row.get(3)?;
        let label: Option<String> = row.get(4)?;
        let msg_count = rmp_serde::from_slice::<Vec<Message>>(&messages_blob)
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(serde_json::json!({
            "session_id": session_id,
            "agent_id": agent_id,
            "message_count": msg_count,
            "created_at": created_at,
            "label": label,
        }))
    }

    /// Create a new empty session for an agent.
    pub fn create_session(&self, agent_id: AgentId) -> OpenCarrierResult<Session> {
        let session = Session {
            id: SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        self.save_session(&session)?;
        Ok(session)
    }

    /// Set the label on an existing session.
    pub fn set_session_label(
        &self,
        session_id: SessionId,
        label: Option<&str>,
    ) -> OpenCarrierResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE sessions SET label = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![label, Utc::now().to_rfc3339(), session_id.0.to_string()],
        )
        .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Find a session by label for a given agent.
    pub fn find_session_by_label(
        &self,
        agent_id: AgentId,
        label: &str,
    ) -> OpenCarrierResult<Option<Session>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, messages, context_window_tokens, label FROM sessions \
                 WHERE agent_id = ?1 AND label = ?2 LIMIT 1",
            )
            .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![agent_id.0.to_string(), label], |row| {
            let id_str: String = row.get(0)?;
            let messages_blob: Vec<u8> = row.get(1)?;
            let tokens: i64 = row.get(2)?;
            let lbl: Option<String> = row.get(3).unwrap_or(None);
            Ok((id_str, messages_blob, tokens, lbl))
        });

        match result {
            Ok((id_str, messages_blob, tokens, lbl)) => {
                let session_id = uuid::Uuid::parse_str(&id_str)
                    .map(SessionId)
                    .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;
                let messages: Vec<Message> = rmp_serde::from_slice(&messages_blob)
                    .map_err(|e| OpenCarrierError::Serialization(e.to_string()))?;
                Ok(Some(Session {
                    id: session_id,
                    agent_id,
                    messages,
                    context_window_tokens: tokens as u64,
                    label: lbl,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(OpenCarrierError::Memory(e.to_string())),
        }
    }
}

impl SessionStore {
    /// List all sessions for a specific agent.
    pub fn list_agent_sessions(
        &self,
        agent_id: AgentId,
    ) -> OpenCarrierResult<Vec<serde_json::Value>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenCarrierError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, messages, created_at, label FROM sessions WHERE agent_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![agent_id.0.to_string()], |row| {
                let session_id: String = row.get(0)?;
                let messages_blob: Vec<u8> = row.get(1)?;
                let created_at: String = row.get(2)?;
                let label: Option<String> = row.get(3)?;
                let msg_count = rmp_serde::from_slice::<Vec<Message>>(&messages_blob)
                    .map(|m| m.len())
                    .unwrap_or(0);
                Ok(serde_json::json!({
                    "session_id": session_id,
                    "message_count": msg_count,
                    "created_at": created_at,
                    "label": label,
                }))
            })
            .map_err(|e| OpenCarrierError::Memory(e.to_string()))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| OpenCarrierError::Memory(e.to_string()))?);
        }
        Ok(sessions)
    }

    /// Create a new session with an optional label.
    pub fn create_session_with_label(
        &self,
        agent_id: AgentId,
        label: Option<&str>,
    ) -> OpenCarrierResult<Session> {
        let session = Session {
            id: SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: label.map(|s| s.to_string()),
        };
        self.save_session(&session)?;
        Ok(session)
    }

    /// Store an LLM-generated summary, replacing older messages with the summary
    /// and keeping only the specified recent messages.
    ///
    /// This is used by the LLM-based compactor to replace text-truncation compaction
    /// with an intelligent, LLM-generated summary of older conversation history.
    ///
    /// Stores the summary and kept messages in the first available session for the agent.
    pub fn store_llm_summary(
        &self,
        agent_id: AgentId,
        summary: &str,
        kept_messages: Vec<Message>,
    ) -> OpenCarrierResult<()> {
        // Find or create a session for this agent to store the summary
        let sessions = self.list_agent_sessions(agent_id)?;
        if let Some(session_info) = sessions.first() {
            if let Some(session_id_str) = session_info.get("session_id").and_then(|v| v.as_str()) {
                if let Ok(session_id) = uuid::Uuid::parse_str(session_id_str).map(SessionId) {
                    if let Ok(Some(mut session)) = self.get_session(session_id) {
                        // Prepend summary as a system message and set kept messages
                        session.messages = kept_messages;
                        self.save_session(&session)?;
                        return Ok(());
                    }
                }
            }
        }
        // No existing session — create one with the kept messages
        let session = Session {
            id: SessionId::new(),
            agent_id,
            messages: kept_messages,
            context_window_tokens: 0,
            label: Some(format!("compacted-{}", summary.len())),
        };
        self.save_session(&session)?;
        Ok(())
    }
}

/// A single JSONL line in the session mirror file.
#[derive(serde::Serialize)]
struct JsonlLine {
    timestamp: String,
    role: String,
    content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use: Option<serde_json::Value>,
}

impl SessionStore {
    /// Write a human-readable JSONL mirror of a session to disk.
    ///
    /// **Append-only**: reads the existing file to find how many lines are
    /// already written, then appends only the new messages. Never truncates
    /// or rewrites existing lines — conversation history is immutable.
    pub fn write_jsonl_mirror(
        &self,
        session: &Session,
        sessions_dir: &Path,
        sender_id: Option<&str>,
    ) -> Result<(), std::io::Error> {
        // Route to per-user sessions directory when sender_id is present
        let effective_dir = if let Some(sid) = sender_id {
            let user_dir = sessions_dir
                .parent()
                .unwrap_or(sessions_dir)
                .join("users")
                .join(sid)
                .join("sessions");
            std::fs::create_dir_all(&user_dir)?;
            user_dir
        } else {
            std::fs::create_dir_all(sessions_dir)?;
            sessions_dir.to_path_buf()
        };
        let path = effective_dir.join(format!("{}.jsonl", session.id.0));

        // Count existing lines to find what's already written
        let existing_lines = if path.exists() {
            std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(&path)?)).count()
        } else {
            0
        };

        // Only append new messages (those beyond what's already written)
        let new_messages = if session.messages.len() > existing_lines {
            &session.messages[existing_lines..]
        } else {
            return Ok(()); // Nothing new to append
        };

        if new_messages.is_empty() {
            return Ok(());
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let now = Utc::now().to_rfc3339();

        for msg in new_messages {
            let role_str = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };

            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_parts: Vec<serde_json::Value> = Vec::new();

            match &msg.content {
                MessageContent::Text(t) => {
                    text_parts.push(t.clone());
                }
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text, .. } => {
                                text_parts.push(text.clone());
                            }
                            ContentBlock::ToolUse {
                                id, name, input, ..
                            } => {
                                tool_parts.push(serde_json::json!({
                                    "type": "tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": input,
                                }));
                            }
                            ContentBlock::ToolResult {
                                tool_use_id,
                                tool_name: _,
                                content,
                                is_error,
                            } => {
                                tool_parts.push(serde_json::json!({
                                    "type": "tool_result",
                                    "tool_use_id": tool_use_id,
                                    "content": content,
                                    "is_error": is_error,
                                }));
                            }
                            ContentBlock::Image { media_type, .. } => {
                                text_parts.push(format!("[image: {media_type}]"));
                            }
                            ContentBlock::Audio { media_type, .. } => {
                                text_parts.push(format!("[audio: {media_type}]"));
                            }
                            ContentBlock::Thinking { thinking } => {
                                text_parts.push(format!(
                                    "[thinking: {}]",
                                    opencarrier_types::truncate_str(thinking, 200)
                                ));
                            }
                            ContentBlock::Unknown => {}
                        }
                    }
                }
            }

            let line = JsonlLine {
                timestamp: now.clone(),
                role: role_str.to_string(),
                content: serde_json::Value::String(text_parts.join("\n")),
                tool_use: if tool_parts.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Array(tool_parts))
                },
            };

            serde_json::to_writer(&mut file, &line).map_err(std::io::Error::other)?;
            file.write_all(b"\n")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;

    fn setup() -> SessionStore {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        SessionStore::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn test_create_and_load_session() {
        let store = setup();
        let agent_id = AgentId::new();
        let session = store.create_session(agent_id).unwrap();

        let loaded = store.get_session(session.id).unwrap().unwrap();
        assert_eq!(loaded.agent_id, agent_id);
        assert!(loaded.messages.is_empty());
    }

    #[test]
    fn test_save_and_load_with_messages() {
        let store = setup();
        let agent_id = AgentId::new();
        let mut session = store.create_session(agent_id).unwrap();
        session.messages.push(Message::user("Hello"));
        session.messages.push(Message::assistant("Hi there!"));
        store.save_session(&session).unwrap();

        let loaded = store.get_session(session.id).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn test_get_missing_session() {
        let store = setup();
        let result = store.get_session(SessionId::new()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_session() {
        let store = setup();
        let agent_id = AgentId::new();
        let session = store.create_session(agent_id).unwrap();
        let sid = session.id;
        assert!(store.get_session(sid).unwrap().is_some());
        store.delete_session(sid).unwrap();
        assert!(store.get_session(sid).unwrap().is_none());
    }

    #[test]
    fn test_delete_agent_sessions() {
        let store = setup();
        let agent_id = AgentId::new();
        let s1 = store.create_session(agent_id).unwrap();
        let s2 = store.create_session(agent_id).unwrap();
        assert!(store.get_session(s1.id).unwrap().is_some());
        assert!(store.get_session(s2.id).unwrap().is_some());
        store.delete_agent_sessions(agent_id).unwrap();
        assert!(store.get_session(s1.id).unwrap().is_none());
        assert!(store.get_session(s2.id).unwrap().is_none());
    }

    #[test]
    fn test_jsonl_mirror_write() {
        let store = setup();
        let agent_id = AgentId::new();
        let mut session = store.create_session(agent_id).unwrap();
        session
            .messages
            .push(opencarrier_types::message::Message::user("Hello"));
        session
            .messages
            .push(opencarrier_types::message::Message::assistant("Hi there!"));
        store.save_session(&session).unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        let sessions_dir = dir.path().join("sessions");
        store
            .write_jsonl_mirror(&session, &sessions_dir, None)
            .unwrap();

        let jsonl_path = sessions_dir.join(format!("{}.jsonl", session.id.0));
        assert!(jsonl_path.exists());

        let content = std::fs::read_to_string(&jsonl_path).unwrap();
        let lines: Vec<&str> = content.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);

        // Verify first line is user message
        let line1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(line1["role"], "user");
        assert_eq!(line1["content"], "Hello");

        // Verify second line is assistant message
        let line2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(line2["role"], "assistant");
        assert_eq!(line2["content"], "Hi there!");
        assert!(line2.get("tool_use").is_none());
    }
}
