//! Append-only session event log — the durable facts behind the
//! model-visible surface (dsh session-log model: "model-visible means
//! logged").
//!
//! Phase A (observational bypass, 2026-08): every surface append is ALSO an
//! event here; the sessions DB table stays authoritative until the P1-C
//! authority flip. This consolidates four half-built event-ish stores into
//! one:
//! - the post-hoc JSONL mirror (written after trimming, line-count aligned —
//!   desyncs on every compaction),
//! - the in-memory-only `turn_log` (discarded when the turn ends),
//! - the never-written `audit` `ToolInvoke` variant,
//! - per-turn `usage_events` aggregates.
//!
//! Tool calls/results live in full fidelity INSIDE the assistant/user
//! message events (blocks are not stripped here, unlike the sessions DB
//! table) — so the old "audit ToolInvoke" role is covered without a second
//! write.
//!
//! Storage: one JSONL file per session under
//! `{db_dir}/session-events/{agent}/{session_id}.jsonl` — kernel-owned,
//! independent of workspace/sender routing. Appends happen at the event
//! points and are never diffed against `session.messages`, so trimming or
//! compacting the in-memory session can never desync the log. (A SQLite
//! index table session → path + last_seq for listing/fold-resume is deferred
//! to the P1-C authority flip, where cross-session queries actually appear.)

use dashmap::DashMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use types::error::{CarrierError, CarrierResult};
use types::message::{Message, MessageContent, Role};

/// One durable session fact. Envelope: per-session monotonic `seq`, epoch-ms
/// `ts_ms`, and a typed `kind`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub ts_ms: i64,
    pub kind: SessionEventKind,
}

/// Durable surface facts (dsh taxonomy: only these reach the model, so only
/// these are logged). Transient per-step injections (status messages, last-run
/// restore, canonical context) are NOT durable surface — they are derived at
/// assembly time and never logged.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SessionEventKind {
    /// A turn opened for this session.
    TurnStart,
    /// A user-role message entered the durable surface (tool_result blocks
    /// included — they ride in user-role messages).
    UserMessage { content: MessageContent },
    /// An assistant message, blocks preserved in full fidelity (text,
    /// tool_use, thinking). The sessions DB table strips these at persist;
    /// the event log is the only place tool history survives.
    AssistantMessage { content: MessageContent },
    /// The turn closed with no user-visible reply (`[[silent]]` / NO_REPLY).
    /// Logged so a skipped attempt leaves a trace (nothing happens
    /// invisibly).
    Silent { reason: String },
    /// Turn envelope close. Absorbs the old in-memory `turn_log` totals.
    TurnEnd {
        iterations: u32,
        tools_called: u32,
        tool_errors: u32,
        outcome: String,
    },
}

/// Map a persisted message batch to surface events.
///
/// `Role::System` messages are skipped: they are transient loop-internal
/// injections (status, corrective nudges), filtered out of history at
/// assembly time — not durable surface.
///
/// Media base64 payloads are redacted (the `url` on image blocks survives):
/// raw bytes are never durable surface — they live in the workspace
/// `input/` files — and writing them into the log would balloon it by
/// megabytes per image.
pub fn message_events(msgs: &[Message]) -> Vec<SessionEventKind> {
    msgs.iter()
        .filter(|m| m.role != Role::System)
        .map(|m| {
            let content = redact_media(&m.content);
            match m.role {
                Role::Assistant => SessionEventKind::AssistantMessage { content },
                _ => SessionEventKind::UserMessage { content },
            }
        })
        .collect()
}

/// Clear base64 media payloads while keeping the block shape and any URL
/// reference (see [`message_events`]).
fn redact_media(content: &MessageContent) -> MessageContent {
    match content {
        MessageContent::Text(t) => MessageContent::Text(t.clone()),
        MessageContent::Blocks(blocks) => MessageContent::Blocks(
            blocks
                .iter()
                .map(|b| match b {
                    types::message::ContentBlock::Image {
                        media_type,
                        data,
                        url,
                    } => types::message::ContentBlock::Image {
                        media_type: media_type.clone(),
                        data: if data.is_empty() {
                            String::new()
                        } else {
                            format!("[redacted: {} bytes base64]", data.len())
                        },
                        url: url.clone(),
                    },
                    types::message::ContentBlock::Audio { media_type, data } => {
                        types::message::ContentBlock::Audio {
                            media_type: media_type.clone(),
                            data: if data.is_empty() {
                                String::new()
                            } else {
                                format!("[redacted: {} bytes base64]", data.len())
                            },
                        }
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
    }
}

/// Append-only JSONL event log writer, one file per session.
///
/// Concurrency: per-session mutex serializes appends and guards the seq
/// counter; the seq is initialized from the file tail on first write per
/// process, so restarts continue the sequence (no seq reuse after crash).
pub struct SessionEventLog {
    base_dir: PathBuf,
    locks: DashMap<String, Arc<Mutex<()>>>,
    last_seq: DashMap<String, u64>,
}

impl SessionEventLog {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            locks: DashMap::new(),
            last_seq: DashMap::new(),
        }
    }

    /// Append `kinds` as sequenced events for `(agent, session)`.
    pub fn append(
        &self,
        agent_id: &str,
        session_id: &str,
        kinds: &[SessionEventKind],
    ) -> CarrierResult<()> {
        if kinds.is_empty() {
            return Ok(());
        }
        let lock = self
            .locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock
            .lock()
            .map_err(|e| CarrierError::Internal(format!("session event lock: {e}")))?;

        let dir = self.base_dir.join(sanitize_component(agent_id));
        std::fs::create_dir_all(&dir).map_err(|e| CarrierError::Memory(e.to_string()))?;
        let path = dir.join(format!("{session_id}.jsonl"));

        let mut seq = match self.last_seq.get(session_id) {
            Some(s) => *s,
            None => {
                let from_file = last_seq_in_file(&path).unwrap_or(0);
                self.last_seq.insert(session_id.to_string(), from_file);
                from_file
            }
        };

        let ts_ms = chrono::Utc::now().timestamp_millis();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        let mut buf = String::new();
        for kind in kinds {
            seq += 1;
            let event = SessionEvent {
                seq,
                ts_ms,
                kind: kind.clone(),
            };
            let line = serde_json::to_string(&event)
                .map_err(|e| CarrierError::Serialization(e.to_string()))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        file.write_all(buf.as_bytes())
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        if let Some(mut s) = self.last_seq.get_mut(session_id) {
            *s = seq;
        } else {
            self.last_seq.insert(session_id.to_string(), seq);
        }
        Ok(())
    }

    /// Read all events for a session (fold input for P1-B).
    pub fn read(&self, agent_id: &str, session_id: &str) -> CarrierResult<Vec<SessionEvent>> {
        let path = self
            .base_dir
            .join(sanitize_component(agent_id))
            .join(format!("{session_id}.jsonl"));
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path).map_err(|e| CarrierError::Memory(e.to_string()))?;
        let mut out = Vec::new();
        for line in std::io::BufReader::new(file).lines() {
            let line = line.map_err(|e| CarrierError::Memory(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(ev) => out.push(ev),
                // A torn final line (crash mid-append) must not kill the fold.
                Err(e) => tracing::warn!(error = %e, "session event line unparseable; skipped"),
            }
        }
        Ok(out)
    }
}

/// Read the seq of the last complete line, or 0 for a fresh/absent file.
/// Seeks near the tail instead of reading the whole file.
fn last_seq_in_file(path: &std::path::Path) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.seek(SeekFrom::End(0)).ok()?;
    let window = len.min(8192);
    file.seek(SeekFrom::Start(len - window)).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut tail = String::new();
    reader.read_to_string(&mut tail).ok()?;
    // Reverse: the very last line may be torn (crash mid-append) — fall back
    // to the previous complete line instead of resetting the seq to 0.
    tail.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find_map(|l| serde_json::from_str::<SessionEvent>(l).ok())
        .map(|ev| ev.seq)
}

/// Path components must not traverse (`..`, `/`). Agent names are kebab-case
/// by construction; this is defense in depth.
fn sanitize_component(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
        }
    }

    #[test]
    fn append_assigns_monotonic_seq_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::new(dir.path().to_path_buf());
        log.append(
            "agent-a",
            "sess-1",
            &[
                SessionEventKind::TurnStart,
                SessionEventKind::UserMessage {
                    content: MessageContent::Text("hi".into()),
                },
            ],
        )
        .unwrap();
        log.append(
            "agent-a",
            "sess-1",
            &[SessionEventKind::Silent {
                reason: "test".into(),
            }],
        )
        .unwrap();

        let events = log.read("agent-a", "sess-1").unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!((events[0].seq, events[2].seq), (1, 3));

        // Simulate a restart: a fresh log over the same dir continues the seq.
        drop(log);
        let log2 = SessionEventLog::new(dir.path().to_path_buf());
        log2.append(
            "agent-a",
            "sess-1",
            &[SessionEventKind::TurnEnd {
                iterations: 1,
                tools_called: 0,
                tool_errors: 0,
                outcome: "complete".into(),
            }],
        )
        .unwrap();
        let events = log2.read("agent-a", "sess-1").unwrap();
        assert_eq!(events.last().unwrap().seq, 4);
    }

    #[test]
    fn message_events_skip_system_and_map_roles() {
        let msgs = vec![
            user_msg("hello"),
            Message::system("transient injection"),
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
            },
        ];
        let events = message_events(&msgs);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], SessionEventKind::UserMessage { .. }));
        assert!(matches!(events[1], SessionEventKind::AssistantMessage { .. }));
    }

    #[test]
    fn tool_blocks_survive_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::new(dir.path().to_path_buf());
        let content = MessageContent::Blocks(vec![types::message::ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "file_read".into(),
            input: serde_json::json!({"path": "大纲.md"}),
            provider_metadata: None,
        }]);
        log.append(
            "agent-a",
            "sess-t",
            &[SessionEventKind::AssistantMessage { content }],
        )
        .unwrap();
        let events = log.read("agent-a", "sess-t").unwrap();
        match &events[0].kind {
            SessionEventKind::AssistantMessage { content } => {
                assert!(matches!(content, MessageContent::Blocks(_)));
            }
            other => panic!("expected AssistantMessage, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_blocks_traversal() {
        // '.' and '/' are both outside the allowed set — the result cannot
        // traverse.
        assert_eq!(sanitize_component("../etc/passwd"), "___etc_passwd");
        assert_eq!(sanitize_component("agent-a"), "agent-a");
    }

    #[test]
    fn media_base64_redacted_url_kept() {
        let msgs = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![types::message::ContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGk=".repeat(1000),
                url: Some("https://example.test/view.png".into()),
            }]),
        }];
        let events = message_events(&msgs);
        match &events[0] {
            SessionEventKind::UserMessage { content } => {
                let serialized = serde_json::to_string(content).unwrap();
                assert!(
                    !serialized.contains("aGk="),
                    "base64 payload must not enter the event log"
                );
                assert!(serialized.contains("redacted"));
                assert!(serialized.contains("view.png"), "url reference survives");
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }
}
