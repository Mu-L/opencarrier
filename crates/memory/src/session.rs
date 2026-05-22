//! Session management — load/save conversation history.

use dashmap::DashMap;
use types::agent::SessionId;
use types::error::{CarrierError, CarrierResult};
use types::message::{ContentBlock, Message, MessageContent, Role};
use chrono::Utc;
use rusqlite::Connection;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A conversation session with message history.
#[derive(Debug, Clone)]
pub struct Session {
    /// Session ID.
    pub id: SessionId,
    /// Owning agent name (stable across restarts).
    pub agent_id: String,
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
    /// Per-session write locks for concurrency-safe append operations.
    session_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionStore {
    /// Create a new session store wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            session_locks: Arc::new(DashMap::new()),
        }
    }

    /// Load a session from the database.
    pub fn get_session(&self, session_id: SessionId) -> CarrierResult<Option<Session>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT agent_id, messages, context_window_tokens, label FROM sessions WHERE id = ?1")
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![session_id.0.to_string()], |row| {
            let agent_str: String = row.get(0)?;
            let messages_blob: Vec<u8> = row.get(1)?;
            let tokens: i64 = row.get(2)?;
            let label: Option<String> = row.get(3).unwrap_or(None);
            Ok((agent_str, messages_blob, tokens, label))
        });

        match result {
            Ok((agent_str, messages_blob, tokens, label)) => {
                let messages: Vec<Message> = rmp_serde::from_slice(&messages_blob)
                    .map_err(|e| CarrierError::Serialization(e.to_string()))?;
                Ok(Some(Session {
                    id: session_id,
                    agent_id: agent_str,
                    messages,
                    context_window_tokens: tokens as u64,
                    label,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CarrierError::Memory(e.to_string())),
        }
    }

    /// Save a session to the database.
    ///
    /// Strips tool_use/tool_result blocks before persisting — these are
    /// execution details needed only during the current agent loop, not
    /// for future conversation continuity.
    pub fn save_session(&self, session: &Session) -> CarrierResult<()> {
        let clean_messages = strip_tool_history(&session.messages);
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let messages_blob = rmp_serde::to_vec_named(&clean_messages)
            .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sessions (id, agent_id, messages, context_window_tokens, label, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET messages = ?3, context_window_tokens = ?4, label = ?5, updated_at = ?6",
            rusqlite::params![
                session.id.0.to_string(),
                &session.agent_id,
                messages_blob,
                session.context_window_tokens as i64,
                session.label.as_deref(),
                now,
            ],
        )
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Append messages to a session (concurrency-safe).
    ///
    /// Acquires a per-session write lock, loads current state from DB,
    /// appends new messages, and saves back. This allows multiple agent
    /// loops to run in parallel for the same agent — each appends its
    /// own new messages without overwriting the other's.
    pub async fn save_session_append(
        &self,
        session_id: SessionId,
        agent_id: &str,
        new_messages: &[Message],
        context_window_tokens: u64,
        label: Option<&str>,
    ) -> CarrierResult<()> {
        let key = session_id.0.to_string();
        let lock = self
            .session_locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let mut session = match self.get_session(session_id)? {
            Some(s) => s,
            None => Session {
                id: session_id,
                agent_id: agent_id.to_string(),
                messages: Vec::new(),
                context_window_tokens: 0,
                label: None,
            },
        };
        session.messages.extend_from_slice(new_messages);
        session.context_window_tokens = context_window_tokens;
        if let Some(l) = label {
            session.label = Some(l.to_string());
        }
        self.save_session(&session)?;

        // Clean up lock entry if no one else is waiting
        drop(_guard);
        self.session_locks.retain(|k, v| Arc::strong_count(v) > 1 || k != &key);
        Ok(())
    }

    /// Delete a session from the database.
    pub fn delete_session(&self, session_id: SessionId) -> CarrierResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            rusqlite::params![session_id.0.to_string()],
        )
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Delete all sessions belonging to an agent.
    pub fn delete_agent_sessions(&self, agent_id: &str) -> CarrierResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        conn.execute(
            "DELETE FROM sessions WHERE agent_id = ?1",
            rusqlite::params![agent_id],
        )
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// List all sessions with metadata (session_id, agent_id, message_count, created_at).
    pub fn list_sessions(&self) -> CarrierResult<Vec<serde_json::Value>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;

        let sql = "SELECT id, agent_id, messages, created_at, label FROM sessions ORDER BY created_at DESC";
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let row_data: Vec<rusqlite::Result<serde_json::Value>> = stmt
            .query_map([], Self::session_row_to_json)
            .map_err(|e| CarrierError::Memory(e.to_string()))?
            .collect();

        let mut sessions = Vec::new();
        for row in row_data {
            sessions.push(row.map_err(|e| CarrierError::Memory(e.to_string()))?);
        }
        Ok(sessions)
    }

    /// List all users (by label) for a given agent, with session stats.
    ///
    /// Groups sessions by their label (format `user:{sender_id}`), returning
    /// each user's sender_id, session count, and last active timestamp.
    pub fn list_agent_users(&self, agent_id: &str) -> CarrierResult<Vec<serde_json::Value>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;

        let sql = "SELECT label, COUNT(*) as session_count, MAX(created_at) as last_active \
                   FROM sessions \
                   WHERE agent_id = ?1 AND label LIKE 'user:%' \
                   GROUP BY label \
                   ORDER BY last_active DESC";
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let rows: Vec<serde_json::Value> = stmt
            .query_map(rusqlite::params![agent_id], |row| {
                let label: String = row.get(0)?;
                let session_count: i64 = row.get(1)?;
                let last_active: String = row.get(2)?;
                Ok((label, session_count, last_active))
            })
            .map_err(|e| CarrierError::Memory(e.to_string()))?
            .filter_map(|r| r.ok())
            .map(|(label, session_count, last_active)| {
                let sender_id = label.strip_prefix("user:").unwrap_or(&label).to_string();
                serde_json::json!({
                    "sender_id": sender_id,
                    "session_count": session_count,
                    "last_active": last_active,
                })
            })
            .collect();

        Ok(rows)
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
    pub fn create_session(&self, agent_id: String) -> CarrierResult<Session> {
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
    ) -> CarrierResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE sessions SET label = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![label, Utc::now().to_rfc3339(), session_id.0.to_string()],
        )
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Find a session by label for a given agent.
    pub fn find_session_by_label(
        &self,
        agent_id: &str,
        label: &str,
    ) -> CarrierResult<Option<Session>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, messages, context_window_tokens, label FROM sessions \
                 WHERE agent_id = ?1 AND label = ?2 LIMIT 1",
            )
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![agent_id, label], |row| {
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
                    .map_err(|e| CarrierError::Memory(e.to_string()))?;
                let messages: Vec<Message> = rmp_serde::from_slice(&messages_blob)
                    .map_err(|e| CarrierError::Serialization(e.to_string()))?;
                Ok(Some(Session {
                    id: session_id,
                    agent_id: agent_id.to_string(),
                    messages,
                    context_window_tokens: tokens as u64,
                    label: lbl,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CarrierError::Memory(e.to_string())),
        }
    }
}

impl SessionStore {
    /// List all sessions for a specific agent.
    pub fn list_agent_sessions(&self, agent_id: &str) -> CarrierResult<Vec<serde_json::Value>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, messages, created_at, label FROM sessions WHERE agent_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![agent_id], |row| {
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
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| CarrierError::Memory(e.to_string()))?);
        }
        Ok(sessions)
    }

    /// Load all sessions + messages for a given agent + sender_id (label = "user:{sender_id}").
    pub fn list_user_sessions(
        &self,
        agent_id: &str,
        sender_id: &str,
    ) -> CarrierResult<Vec<(String, Vec<Message>)>> {
        let label = format!("user:{}", sender_id);
        let conn = self
            .conn
            .lock()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, messages FROM sessions \
                 WHERE agent_id = ?1 AND label = ?2 \
                 ORDER BY created_at ASC",
            )
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![agent_id, label], |row| {
                let session_id: String = row.get(0)?;
                let messages_blob: Vec<u8> = row.get(1)?;
                Ok((session_id, messages_blob))
            })
            .map_err(|e| CarrierError::Memory(e.to_string()))?;

        let mut result = Vec::new();
        for row in rows {
            let (session_id, messages_blob) = row.map_err(|e| CarrierError::Memory(e.to_string()))?;
            let messages: Vec<Message> = rmp_serde::from_slice(&messages_blob)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?;
            result.push((session_id, messages));
        }
        Ok(result)
    }

    /// Create a new session with an optional label.
    pub fn create_session_with_label(
        &self,
        agent_id: String,
        label: Option<&str>,
    ) -> CarrierResult<Session> {
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
        owner_id: Option<&str>,
        sender_id: Option<&str>,
        home_dir: Option<&Path>,
        agent_name: Option<&str>,
    ) -> Result<(), std::io::Error> {
        // Route to per-sender sessions directory when sender_id is present
        let effective_dir = if let (Some(oid), Some(hd), Some(an)) = (owner_id.or(sender_id), home_dir, agent_name) {
            let user_dir = types::config::sender_data_dir(hd, oid, an, sender_id).join("sessions");
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
                                    types::truncate_str(thinking, 200)
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

/// Strip tool_use/tool_result blocks from messages before persisting to DB.
///
/// The purpose of the session is to maintain conversational continuity
/// (what the user asked, what the assistant responded). Tool calls and
/// results are execution ephemera — needed during the current agent loop
/// but useless noise for future turns. Stripping them prevents context
/// bloat (especially from large tool results like base64 image data).
///
/// After stripping:
/// - User messages: kept as-is
/// - Assistant messages: keep only text/thinking blocks, drop tool_use
/// - Messages that become empty after stripping are removed entirely
fn strip_tool_history(messages: &[Message]) -> Vec<Message> {
    let mut clean = Vec::with_capacity(messages.len());
    for msg in messages {
        match &msg.content {
            MessageContent::Text(_) => {
                // Plain text messages always kept
                clean.push(msg.clone());
            }
            MessageContent::Blocks(blocks) => {
                let mut has_tool_use = false;
                let mut kept_blocks: Vec<ContentBlock> = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::ToolUse { name, input, .. } => {
                            has_tool_use = true;
                            // Replace tool_use with a brief summary line
                            let summary = format!("[Called {name}]");
                            kept_blocks.push(ContentBlock::Text {
                                text: summary,
                                provider_metadata: None,
                            });
                            // Log input size for debugging (not persisted)
                            let _ = input;
                        }
                        ContentBlock::ToolResult { tool_name, is_error, .. } => {
                            // Replace tool_result with a brief placeholder
                            let marker = if *is_error { " (error)" } else { "" };
                            let summary = format!("[Result from {tool_name}{marker}]");
                            kept_blocks.push(ContentBlock::Text {
                                text: summary,
                                provider_metadata: None,
                            });
                        }
                        ContentBlock::Image { .. } | ContentBlock::Audio { .. } => {
                            // Drop inline media — too large for persistence
                        }
                        other => {
                            kept_blocks.push(other.clone());
                        }
                    }
                }

                if kept_blocks.is_empty() && !has_tool_use {
                    // Message with only images/audio — skip entirely
                    continue;
                }

                // If all blocks were tool-use related but we have summaries, keep them
                // If the message had text alongside tool_use, the text is preserved
                if !kept_blocks.is_empty() {
                    clean.push(Message {
                        role: msg.role,
                        content: MessageContent::Blocks(kept_blocks),
                    });
                }
            }
        }
    }
    clean
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
        let agent_id = "test-agent".to_string();
        let session = store.create_session(agent_id.clone()).unwrap();

        let loaded = store.get_session(session.id).unwrap().unwrap();
        assert_eq!(loaded.agent_id, agent_id);
        assert!(loaded.messages.is_empty());
    }

    #[test]
    fn test_save_and_load_with_messages() {
        let store = setup();
        let agent_id = "test-agent".to_string();
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
        let agent_id = "test-agent".to_string();
        let session = store.create_session(agent_id).unwrap();
        let sid = session.id;
        assert!(store.get_session(sid).unwrap().is_some());
        store.delete_session(sid).unwrap();
        assert!(store.get_session(sid).unwrap().is_none());
    }

    #[test]
    fn test_delete_agent_sessions() {
        let store = setup();
        let agent_id = "test-agent".to_string();
        let s1 = store.create_session(agent_id.clone()).unwrap();
        let s2 = store.create_session(agent_id.clone()).unwrap();
        assert!(store.get_session(s1.id).unwrap().is_some());
        assert!(store.get_session(s2.id).unwrap().is_some());
        store.delete_agent_sessions(&agent_id).unwrap();
        assert!(store.get_session(s1.id).unwrap().is_none());
        assert!(store.get_session(s2.id).unwrap().is_none());
    }

    #[test]
    fn test_jsonl_mirror_write() {
        let store = setup();
        let agent_id = "test-agent".to_string();
        let mut session = store.create_session(agent_id).unwrap();
        session
            .messages
            .push(types::message::Message::user("Hello"));
        session
            .messages
            .push(types::message::Message::assistant("Hi there!"));
        store.save_session(&session).unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        let sessions_dir = dir.path().join("sessions");
        store
            .write_jsonl_mirror(&session, &sessions_dir, None, None, None, None)
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

    #[test]
    fn test_strip_tool_history_removes_tool_blocks() {
        let messages = vec![
            Message::user("Generate an image"),
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "tu1".to_string(),
                        name: "image_generate".to_string(),
                        input: serde_json::json!({"prompt": "a cat"}),
                        provider_metadata: None,
                    },
                ]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "tu1".to_string(),
                        tool_name: "image_generate".to_string(),
                        content: "huge base64 data here...".repeat(1000),
                        is_error: false,
                    },
                ]),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Image generated successfully".to_string(),
                        provider_metadata: None,
                    },
                ]),
            },
        ];

        let clean = super::strip_tool_history(&messages);

        // Should have 4 messages (none removed entirely — tool blocks replaced with summaries)
        assert_eq!(clean.len(), 4);

        // First message unchanged
        assert_eq!(clean[0].role, Role::User);

        // Tool_use replaced with text summary
        if let MessageContent::Blocks(blocks) = &clean[1].content {
            assert!(blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("Called image_generate"))));
            assert!(!blocks.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. })));
        } else {
            panic!("Expected Blocks");
        }

        // Tool_result replaced with text summary
        if let MessageContent::Blocks(blocks) = &clean[2].content {
            assert!(blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("Result from"))));
            assert!(!blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })));
        } else {
            panic!("Expected Blocks");
        }

        // Final assistant text preserved
        if let MessageContent::Blocks(blocks) = &clean[3].content {
            assert!(blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("successfully"))));
        }
    }

    #[test]
    fn test_save_session_strips_tools() {
        let store = setup();
        let mut session = store.create_session("test-agent".to_string()).unwrap();

        // Add messages with tool blocks
        session.messages.push(Message::user("Hello"));
        session.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolUse {
                    id: "tu1".to_string(),
                    name: "some_tool".to_string(),
                    input: serde_json::json!({}),
                    provider_metadata: None,
                },
            ]),
        });
        session.messages.push(Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tu1".to_string(),
                    tool_name: "some_tool".to_string(),
                    content: "big result data".repeat(1000),
                    is_error: false,
                },
            ]),
        });

        store.save_session(&session).unwrap();

        // Reload — tool blocks should be stripped
        let loaded = store.get_session(session.id).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 3);

        // No ToolUse or ToolResult blocks in loaded messages
        for msg in &loaded.messages {
            if let MessageContent::Blocks(blocks) = &msg.content {
                for block in blocks {
                    assert!(!matches!(block, ContentBlock::ToolUse { .. }), "ToolUse should be stripped");
                    assert!(!matches!(block, ContentBlock::ToolResult { .. }), "ToolResult should be stripped");
                }
            }
        }
    }
}
