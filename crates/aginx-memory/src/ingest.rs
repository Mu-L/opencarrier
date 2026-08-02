//! Ingest orchestrator (PG-backed): canonicalise -> chunk -> score -> persist
//! -> enqueue extract jobs.
//!
//! Ported from `memory::tree::ingest::IngestPipeline` - same hot path, stores
//! swapped to the PG async stores and `ingest` is `async`. The pure helpers
//! (`canonicalize`, `chunker`, `scoring`, `extract`) are reused verbatim from
//! the memory crate.

use std::path::PathBuf;

use deadpool_postgres::Pool;
use memory::tree::canonicalize::{self, CanonicalisedSource};
use memory::tree::chunker::{self, ChunkInput};
use memory::tree::content_store::ContentStore;
use memory::tree::entity_store::EntityIndexEntry;
use memory::tree::extract;
use memory::tree::scoring;
use memory::tree::types::{
    ExtractChunkPayload, JobKind, NewJob, SourceKind, CHUNK_STATUS_PENDING_EXTRACTION,
};
use types::error::{CarrierError, CarrierResult};
use types::memory_tree::{IngestRequest, IngestResult};

use crate::pg::chunk_store::ChunkStore;
use crate::pg::entity_store::EntityStore;
use crate::pg::job_store::JobStore;
use crate::pg::score_store::ScoreStore;

/// Ingest pipeline backed by PG + filesystem content store.
#[derive(Clone)]
pub struct IngestPipeline {
    chunk_store: ChunkStore,
    score_store: ScoreStore,
    entity_store: EntityStore,
    job_store: JobStore,
    content_store: ContentStore,
}

impl IngestPipeline {
    pub fn new(pool: Pool, content_root: PathBuf) -> Self {
        Self {
            chunk_store: ChunkStore::new(pool.clone()),
            score_store: ScoreStore::new(pool.clone()),
            entity_store: EntityStore::new(pool.clone()),
            job_store: JobStore::new(pool.clone()),
            content_store: ContentStore::new(content_root),
        }
    }

    /// Ingest a batch of messages from any source kind.
    ///
    /// Chat and email: no source-level gate (streams accept repeated batches).
    /// Document: deduped by (owner_id, source_kind, source_id).
    pub async fn ingest(&self, req: &IngestRequest) -> CarrierResult<IngestResult> {
        let source_kind = parse_source_kind(&req.source_kind);

        // Document dedup: skip if already ingested
        if source_kind == SourceKind::Document
            && self
                .job_store
                .check_ingested(&req.owner_id, &req.source_kind, &req.source_id)
                .await?
        {
            return Ok(IngestResult {
                chunks_created: 0,
                chunks_dropped: 0,
                source_id: req.source_id.clone(),
            });
        }

        // Canonicalise based on source kind
        let canonical = match source_kind {
            SourceKind::Chat => canonicalise_chat(req),
            SourceKind::Email => canonicalise_email(req),
            SourceKind::Document => canonicalise_document(req),
        };

        let canonical = match canonical {
            Some(c) => c,
            None => {
                return Ok(IngestResult {
                    chunks_created: 0,
                    chunks_dropped: 0,
                    source_id: req.source_id.clone(),
                });
            }
        };

        // Chunk the canonical markdown
        let tags = req.tags.clone();
        let chunks = chunker::chunk_messages(&ChunkInput {
            owner_id: &req.owner_id,
            user_id: req.user_id.as_deref().unwrap_or(""),
            agent_id: &req.agent_id,
            source_kind,
            source_id: &req.source_id,
            source_ref: canonical.source_ref.as_deref(),
            markdown: &canonical.markdown,
            tags: &tags,
            timestamp_ms: canonical.first_ts_ms,
            max_tokens: memory::tree::types::DEFAULT_CHUNK_MAX_TOKENS,
        });

        if chunks.is_empty() {
            return Ok(IngestResult {
                chunks_created: 0,
                chunks_dropped: 0,
                source_id: req.source_id.clone(),
            });
        }

        // Ensure content directories exist
        self.content_store.ensure_dirs(&req.owner_id)?;

        // Score each chunk and classify
        let mut chunks_written = 0usize;
        let mut chunks_dropped = 0usize;

        for chunk in &chunks {
            // Extract entities for scoring
            let entities = extract::extract_entities(&chunk.content);
            let entity_count = entities.len();

            // Score
            let decision = scoring::score_chunk(&chunk.content, source_kind, &tags, entity_count);

            // Persist score row
            self.score_store
                .write_score(
                    &req.owner_id,
                    &chunk.id,
                    &decision.signals,
                    decision.total,
                    decision.dropped,
                    Some(&decision.reason),
                )
                .await?;

            if decision.dropped {
                // Persist chunk but mark as dropped
                self.chunk_store
                    .upsert_chunks(std::slice::from_ref(chunk))
                    .await?;
                self.chunk_store
                    .update_lifecycle(&req.owner_id, &chunk.id, "dropped")
                    .await?;
                chunks_dropped += 1;
                continue;
            }

            // Persist chunk content to disk
            self.content_store.write_chunk(&req.owner_id, chunk)?;

            // Persist chunk to PG
            self.chunk_store
                .upsert_chunks(std::slice::from_ref(chunk))
                .await?;
            self.chunk_store
                .update_lifecycle(&req.owner_id, &chunk.id, CHUNK_STATUS_PENDING_EXTRACTION)
                .await?;

            // Persist entity index entries
            for entity in &entities {
                let entry = EntityIndexEntry {
                    entity_id: &entity.canonical_id,
                    node_id: &chunk.id,
                    node_kind: "leaf",
                    entity_kind: entity.kind,
                    surface: &entity.surface,
                    score: decision.total,
                    timestamp_ms: chunk.timestamp_ms,
                    tree_id: None,
                    user_id: chunk.user_id.as_str(),
                };
                self.entity_store
                    .upsert_entity_index(&req.owner_id, &entry)
                    .await?;
                // Bump entity hotness
                self.entity_store
                    .bump_entity_hotness(&req.owner_id, &entity.canonical_id, &req.source_id)
                    .await?;
            }

            // Enqueue ExtractChunk job
            let payload = ExtractChunkPayload {
                chunk_id: chunk.id.clone(),
            };
            let job = NewJob {
                owner_id: req.owner_id.clone(),
                kind: JobKind::ExtractChunk,
                payload_json: serde_json::to_string(&payload)
                    .map_err(|e| CarrierError::Internal(e.to_string()))?,
                dedupe_key: Some(format!("extract:{}", chunk.id)),
                available_at_ms: None,
                max_attempts: None,
            };
            self.job_store.enqueue(&job).await?;

            chunks_written += 1;
        }

        // Mark document sources as ingested
        if source_kind == SourceKind::Document && chunks_written > 0 {
            self.job_store
                .mark_ingested(&req.owner_id, &req.source_kind, &req.source_id)
                .await?;
        }

        Ok(IngestResult {
            chunks_created: chunks_written,
            chunks_dropped,
            source_id: req.source_id.clone(),
        })
    }
}

/// Parse source_kind string into enum.
fn parse_source_kind(s: &str) -> SourceKind {
    match s {
        "chat" => SourceKind::Chat,
        "email" => SourceKind::Email,
        "document" => SourceKind::Document,
        _ => SourceKind::Chat, // default
    }
}

fn canonicalise_chat(req: &IngestRequest) -> Option<CanonicalisedSource> {
    let messages: Vec<canonicalize::chat::ChatMessage> = req
        .messages
        .iter()
        .map(|m| canonicalize::chat::ChatMessage {
            author: m.sender.clone(),
            timestamp_ms: m.timestamp_ms,
            text: m.content.clone(),
            source_ref: None,
        })
        .collect();

    canonicalize::chat::canonicalise(
        &req.source_id,
        &req.tags,
        canonicalize::chat::ChatBatch {
            platform: req.source_kind.clone(),
            channel_label: req.source_id.clone(),
            messages,
        },
    )
}

fn canonicalise_email(req: &IngestRequest) -> Option<CanonicalisedSource> {
    let messages: Vec<canonicalize::email::EmailMessage> = req
        .messages
        .iter()
        .map(|m| canonicalize::email::EmailMessage {
            from: m.sender.clone(),
            to: vec![],
            cc: vec![],
            subject: String::new(),
            sent_at_ms: m.timestamp_ms,
            body: m.content.clone(),
            source_ref: None,
        })
        .collect();

    canonicalize::email::canonicalise(
        &req.source_id,
        &req.tags,
        canonicalize::email::EmailThread {
            provider: req.source_kind.clone(),
            thread_subject: String::new(),
            messages,
        },
    )
}

fn canonicalise_document(req: &IngestRequest) -> Option<CanonicalisedSource> {
    let body = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let modified_at_ms = req
        .messages
        .first()
        .map(|m| m.timestamp_ms)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

    canonicalize::document::canonicalise(
        &req.source_id,
        &req.tags,
        canonicalize::document::DocumentInput {
            provider: req.source_kind.clone(),
            title: String::new(),
            body,
            modified_at_ms,
            source_ref: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;
    use tempfile::TempDir;
    use types::memory_tree::IngestMessage;

    async fn setup() -> Option<(Pool, TempDir)> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        let dir = TempDir::new().ok()?;
        Some((pool, dir))
    }

    fn chat_request(owner_id: &str, source_id: &str, messages: Vec<(&str, &str, i64)>) -> IngestRequest {
        IngestRequest {
            owner_id: owner_id.to_string(),
            agent_id: "agent_1".to_string(),
            source_kind: "chat".to_string(),
            source_id: source_id.to_string(),
            messages: messages
                .into_iter()
                .map(|(sender, content, ts_ms)| IngestMessage {
                    sender: sender.to_string(),
                    content: content.to_string(),
                    timestamp_ms: ts_ms,
                })
                .collect(),
            tags: vec![],
            user_id: None,
        }
    }

    #[tokio::test]
    async fn ingest_chat_creates_chunks() {
        let (pool, dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let pipeline = IngestPipeline::new(pool, dir.path().to_path_buf());
        let req = chat_request(
            "owner_1",
            "wechat:gh_abc:sender_1",
            vec![
                ("Alice", "We are planning to ship the Phoenix migration on Friday after reviewing the runbook. alice@example.com", 1_700_000_000_000),
                ("Bob", "Confirmed, I will handle the coordination and launch tracking tonight.", 1_700_000_010_000),
            ],
        );
        let result = pipeline.ingest(&req).await.unwrap();
        assert!(result.chunks_created >= 1);
        assert_eq!(result.source_id, "wechat:gh_abc:sender_1");
    }

    #[tokio::test]
    async fn ingest_empty_messages() {
        let (pool, dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let pipeline = IngestPipeline::new(pool, dir.path().to_path_buf());
        let req = chat_request("owner_1", "wechat:gh_abc:sender_1", vec![]);
        let result = pipeline.ingest(&req).await.unwrap();
        assert_eq!(result.chunks_created, 0);
        assert_eq!(result.chunks_dropped, 0);
    }

    #[tokio::test]
    async fn ingest_document_dedup() {
        let (pool, dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let pipeline = IngestPipeline::new(pool, dir.path().to_path_buf());
        let req = IngestRequest {
            owner_id: "owner_1".to_string(),
            agent_id: "agent_1".to_string(),
            source_kind: "document".to_string(),
            source_id: "notion:page_abc".to_string(),
            messages: vec![IngestMessage {
                sender: "system".to_string(),
                content: "Important document content about project phoenix.".to_string(),
                timestamp_ms: 1_700_000_000_000,
            }],
            tags: vec![],
            user_id: None,
        };
        let first = pipeline.ingest(&req).await.unwrap();
        assert!(first.chunks_created >= 1);
        // Second ingest of same document should be deduped.
        let second = pipeline.ingest(&req).await.unwrap();
        assert_eq!(second.chunks_created, 0);
    }

    #[tokio::test]
    async fn ingest_owner_isolation() {
        let (pool, dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let pipeline = IngestPipeline::new(pool, dir.path().to_path_buf());
        let req1 = chat_request(
            "owner_1",
            "wechat:gh_abc:sender_1",
            vec![("Alice", "We are planning to ship the Phoenix migration on Friday after reviewing the runbook.", 1_700_000_000_000)],
        );
        let req2 = chat_request(
            "owner_2",
            "wechat:gh_abc:sender_1",
            vec![("Alice", "We are planning to ship the Phoenix migration on Friday after reviewing the runbook.", 1_700_000_000_000)],
        );
        let r1 = pipeline.ingest(&req1).await.unwrap();
        let r2 = pipeline.ingest(&req2).await.unwrap();
        // Same content, different owners -> different chunk IDs (owner_id is in hash)
        assert!(r1.chunks_created >= 1);
        assert!(r2.chunks_created >= 1);
    }

    #[tokio::test]
    async fn ingest_enqueues_extract_jobs() {
        let (pool, dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let pipeline = IngestPipeline::new(pool.clone(), dir.path().to_path_buf());
        let req = chat_request(
            "owner_1",
            "wechat:gh_abc:sender_1",
            vec![
                ("Alice", "We are planning to ship the Phoenix migration on Friday after reviewing the runbook. alice@example.com", 1_700_000_000_000),
                ("Bob", "Confirmed, I will handle the coordination and launch tracking tonight.", 1_700_000_010_000),
            ],
        );
        let result = pipeline.ingest(&req).await.unwrap();
        // The ExtractChunk jobs for created chunks should be queued.
        let pending = JobStore::new(pool)
            .count_pending("owner_1", Some(JobKind::ExtractChunk))
            .await
            .unwrap();
        assert!(pending >= 1, "extract jobs should be enqueued, got {pending}");
        assert!(result.chunks_created >= 1);
    }
}
