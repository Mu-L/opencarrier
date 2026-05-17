//! Per-`JobKind` handler implementations dispatched by the worker pool.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use types::error::CarrierResult;
use types::memory_tree::TreeKind;

use crate::tree::bucket_seal::BucketSealEngine;
use crate::tree::entity_store::{EntityIndexEntry, EntityStore};
use crate::tree::extract::extract_entities;
use crate::tree::job_store::JobStore;
use crate::tree::store::ChunkStore;
use crate::tree::summariser::inert::InertSummariser;
use crate::tree::tree_global::digest::{self, DigestOutcome};
use crate::tree::tree_store::TreeTreeStore;
use crate::tree::tree_topic::routing::route_leaf_to_topic_trees;
use crate::tree::types::{
    AppendBufferPayload, AppendTarget, ExtractChunkPayload, FlushStalePayload,
    Job, JobKind, NodeRef, SealPayload, TopicRoutePayload, DEFAULT_FLUSH_AGE_SECS,
};

/// Outcome of a successful handler run.
#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    Done,
    Defer { until_ms: i64, reason: String },
}

/// Dispatch a claimed job to the matching per-kind handler.
pub fn handle_job(
    conn: &Arc<Mutex<Connection>>,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    match job.kind {
        JobKind::ExtractChunk => handle_extract(conn, content_root, owner_id, job),
        JobKind::AppendBuffer => handle_append_buffer(conn, content_root, owner_id, job),
        JobKind::Seal => handle_seal(conn, content_root, owner_id, job),
        JobKind::TopicRoute => handle_topic_route(conn, content_root, owner_id, job),
        JobKind::DigestDaily => handle_digest_daily(conn, content_root, owner_id, job),
        JobKind::FlushStale => handle_flush_stale(conn, content_root, owner_id, job),
    }
}

fn handle_extract(
    conn: &Arc<Mutex<Connection>>,
    _content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: ExtractChunkPayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| types::error::CarrierError::Internal(format!("parse ExtractChunk payload: {e}")))?;

    let chunk_store = ChunkStore::new(conn.clone());
    let entity_store = EntityStore::new(conn.clone());
    let job_store = JobStore::new(conn.clone());

    let Some(chunk) = chunk_store.get_chunk(owner_id, &payload.chunk_id)? else {
        tracing::warn!(
            "[tree_jobs] extract chunk missing chunk_id={}",
            payload.chunk_id
        );
        return Ok(JobOutcome::Done);
    };

    // Extract entities from chunk content
    let extracted = extract_entities(&chunk.content);
    let entity_ids: Vec<String> = extracted.iter().map(|e| e.canonical_id.clone()).collect();

    // Index entities
    for entity in &extracted {
        let entry = EntityIndexEntry {
            entity_id: &entity.canonical_id,
            node_id: &chunk.id,
            node_kind: "leaf",
            entity_kind: entity.kind,
            surface: &entity.surface,
            score: 0.0,
            timestamp_ms: chunk.timestamp_ms,
            tree_id: None,
        };
        entity_store.upsert_entity_index(owner_id, &entry)?;
    }

    // Mark as admitted
    chunk_store.update_lifecycle(owner_id, &chunk.id, "admitted")?;

    // Enqueue follow-up: AppendBuffer to source tree
    let append_payload = AppendBufferPayload {
        node: NodeRef::Leaf {
            chunk_id: chunk.id.clone(),
        },
        target: AppendTarget::Source {
            source_id: chunk.source_id.clone(),
        },
    };
    let dedupe_key = format!(
        "append:source:{}:leaf:{}",
        chunk.source_id, chunk.id
    );
    let new_job = crate::tree::types::NewJob {
        owner_id: owner_id.to_string(),
        kind: JobKind::AppendBuffer,
        payload_json: serde_json::to_string(&append_payload)
            .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
        dedupe_key: Some(dedupe_key),
        available_at_ms: None,
        max_attempts: None,
    };
    job_store.enqueue(&new_job)?;

    // Enqueue follow-up: TopicRoute
    if !entity_ids.is_empty() {
        let route_payload = TopicRoutePayload {
            node: NodeRef::Leaf {
                chunk_id: chunk.id.clone(),
            },
        };
        let dedupe_key = format!("topic_route:leaf:{}", chunk.id);
        let new_job = crate::tree::types::NewJob {
            owner_id: owner_id.to_string(),
            kind: JobKind::TopicRoute,
            payload_json: serde_json::to_string(&route_payload)
                .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job)?;
    }

    Ok(JobOutcome::Done)
}

fn handle_append_buffer(
    conn: &Arc<Mutex<Connection>>,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: AppendBufferPayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| types::error::CarrierError::Internal(format!("parse AppendBuffer payload: {e}")))?;

    let tree_store = TreeTreeStore::new(conn.clone());
    let chunk_store = ChunkStore::new(conn.clone());
    let job_store = JobStore::new(conn.clone());

    // Resolve the tree for the target
    let tree = match &payload.target {
        AppendTarget::Source { source_id } => {
            tree_store.get_or_create_tree(owner_id, TreeKind::Source, source_id)?
        }
        AppendTarget::Topic { tree_id } => {
            match tree_store.get_tree(owner_id, tree_id)? {
                Some(t) => t,
                None => {
                    tracing::warn!(
                        "[tree_jobs] append_buffer topic tree missing tree_id={tree_id}"
                    );
                    return Ok(JobOutcome::Done);
                }
            }
        }
    };

    // Get token count and timestamp from the node
    let (item_id, token_count, timestamp_ms) = match &payload.node {
        NodeRef::Leaf { chunk_id } => {
            let Some(chunk) = chunk_store.get_chunk(owner_id, chunk_id)? else {
                tracing::warn!(
                    "[tree_jobs] append_buffer chunk missing chunk_id={chunk_id}"
                );
                return Ok(JobOutcome::Done);
            };
            (chunk.id.clone(), chunk.token_count, chunk.timestamp_ms)
        }
        NodeRef::Summary { summary_id } => {
            let Some(summary) = tree_store.get_summary(owner_id, summary_id)? else {
                tracing::warn!(
                    "[tree_jobs] append_buffer summary missing summary_id={summary_id}"
                );
                return Ok(JobOutcome::Done);
            };
            (summary.id.clone(), summary.token_count, summary.time_range_start_ms)
        }
    };

    let seal_engine = BucketSealEngine::new(
        conn.clone(),
        content_root.to_path_buf(),
        Arc::new(InertSummariser),
    );

    // Append to L0 buffer
    seal_engine.append_to_buffer(owner_id, &tree.id, 0, &item_id, token_count as i64, timestamp_ms)?;

    // Check if seal should happen
    let buf = seal_engine.get_or_create_buffer(owner_id, &tree.id, 0)?;
    if crate::tree::bucket_seal::should_seal(&buf) {
        let seal_payload = SealPayload {
            tree_id: tree.id.clone(),
            level: 0,
            force_now_ms: None,
        };
        let dedupe_key = format!("seal:{}:0", tree.id);
        let new_job = crate::tree::types::NewJob {
            owner_id: owner_id.to_string(),
            kind: JobKind::Seal,
            payload_json: serde_json::to_string(&seal_payload)
                .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job)?;
    }

    // Update lifecycle for source-target leaf chunks
    if matches!(payload.target, AppendTarget::Source { .. }) {
        if let NodeRef::Leaf { chunk_id } = &payload.node {
            chunk_store.update_lifecycle(owner_id, chunk_id, "buffered")?;
        }
    }

    Ok(JobOutcome::Done)
}

fn handle_seal(
    conn: &Arc<Mutex<Connection>>,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: SealPayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| types::error::CarrierError::Internal(format!("parse Seal payload: {e}")))?;

    let tree_store = TreeTreeStore::new(conn.clone());
    let job_store = JobStore::new(conn.clone());

    let Some(tree) = tree_store.get_tree(owner_id, &payload.tree_id)? else {
        tracing::warn!(
            "[tree_jobs] seal tree missing tree_id={}",
            payload.tree_id
        );
        return Ok(JobOutcome::Done);
    };

    let seal_engine = BucketSealEngine::new(
        conn.clone(),
        content_root.to_path_buf(),
        Arc::new(InertSummariser),
    );

    let forced = payload.force_now_ms.is_some();
    let sealed_ids = seal_engine.cascade_seals(owner_id, &tree, payload.level, forced)?;

    // For source trees, enqueue TopicRoute for each new summary
    if tree.kind == TreeKind::Source {
        for summary_id in &sealed_ids {
            let route_payload = TopicRoutePayload {
                node: NodeRef::Summary {
                    summary_id: summary_id.clone(),
                },
            };
            let dedupe_key = format!("topic_route:summary:{summary_id}");
            let new_job = crate::tree::types::NewJob {
                owner_id: owner_id.to_string(),
                kind: JobKind::TopicRoute,
                payload_json: serde_json::to_string(&route_payload)
                    .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
                dedupe_key: Some(dedupe_key),
                available_at_ms: None,
                max_attempts: None,
            };
            job_store.enqueue(&new_job)?;
        }
    }

    Ok(JobOutcome::Done)
}

fn handle_topic_route(
    conn: &Arc<Mutex<Connection>>,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: TopicRoutePayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| types::error::CarrierError::Internal(format!("parse TopicRoute payload: {e}")))?;

    let tree_store = TreeTreeStore::new(conn.clone());
    let entity_store = EntityStore::new(conn.clone());

    // Get entity IDs for the node
    let node_id = match &payload.node {
        NodeRef::Leaf { chunk_id } => chunk_id.clone(),
        NodeRef::Summary { summary_id } => summary_id.clone(),
    };

    let entity_ids = entity_store.entities_for_node(owner_id, &node_id)?;
    if entity_ids.is_empty() {
        return Ok(JobOutcome::Done);
    }

    // Get token count and timestamp for routing
    let (token_count, timestamp_ms) = match &payload.node {
        NodeRef::Leaf { chunk_id } => {
            let chunk_store = ChunkStore::new(conn.clone());
            if let Some(chunk) = chunk_store.get_chunk(owner_id, chunk_id)? {
                (chunk.token_count, chunk.timestamp_ms)
            } else {
                return Ok(JobOutcome::Done);
            }
        }
        NodeRef::Summary { summary_id } => {
            if let Some(summary) = tree_store.get_summary(owner_id, summary_id)? {
                (summary.token_count, summary.time_range_start_ms)
            } else {
                return Ok(JobOutcome::Done);
            }
        }
    };

    let item_id = node_id.clone();
    route_leaf_to_topic_trees(
        conn,
        content_root,
        owner_id,
        &item_id,
        token_count,
        timestamp_ms,
        &entity_ids,
    )?;

    Ok(JobOutcome::Done)
}

fn handle_digest_daily(
    conn: &Arc<Mutex<Connection>>,
    content_root: &Path,
    owner_id: &str,
    _job: &Job,
) -> CarrierResult<JobOutcome> {
    match digest::end_of_day_digest(conn, content_root, owner_id, &InertSummariser)? {
        DigestOutcome::Emitted { daily_id, .. } => {
            tracing::info!("[tree_jobs] emitted digest daily_id={daily_id}");
        }
        DigestOutcome::EmptyDay => {}
        DigestOutcome::Skipped { existing_id } => {
            tracing::debug!("[tree_jobs] digest skipped existing_id={existing_id}");
        }
    }
    Ok(JobOutcome::Done)
}

fn handle_flush_stale(
    conn: &Arc<Mutex<Connection>>,
    _content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: FlushStalePayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| types::error::CarrierError::Internal(format!("parse FlushStale payload: {e}")))?;

    let age_secs = payload.max_age_secs.unwrap_or(DEFAULT_FLUSH_AGE_SECS);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cutoff_ms = now_ms - (age_secs * 1000);

    let tree_store = TreeTreeStore::new(conn.clone());
    let job_store = JobStore::new(conn.clone());

    // Find buffers with items older than cutoff
    let stale_buffers = tree_store.list_stale_buffers(owner_id, cutoff_ms)?;

    for buf in stale_buffers {
        let seal_payload = SealPayload {
            tree_id: buf.tree_id.clone(),
            level: buf.level,
            force_now_ms: Some(now_ms),
        };
        let dedupe_key = format!("seal:{}:{}", buf.tree_id, buf.level);
        let new_job = crate::tree::types::NewJob {
            owner_id: owner_id.to_string(),
            kind: JobKind::Seal,
            payload_json: serde_json::to_string(&seal_payload)
                .map_err(|e| types::error::CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job)?;
    }

    Ok(JobOutcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use crate::tree::types::{DigestDailyPayload, SourceKind};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn setup() -> (Arc<Mutex<Connection>>, PathBuf, TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let dir = TempDir::new().unwrap();
        (Arc::new(Mutex::new(conn)), dir.path().to_path_buf(), dir)
    }

    fn mk_job(owner_id: &str, kind: JobKind, payload_json: &str) -> Job {
        let now_ms = chrono::Utc::now().timestamp_millis();
        Job {
            id: "test-job-id".to_string(),
            owner_id: owner_id.to_string(),
            kind,
            payload_json: payload_json.to_string(),
            dedupe_key: None,
            status: crate::tree::types::JobStatus::Running,
            attempts: 1,
            max_attempts: 5,
            available_at_ms: now_ms,
            locked_until_ms: Some(now_ms + 60_000),
            last_error: None,
            created_at_ms: now_ms,
            started_at_ms: Some(now_ms),
            completed_at_ms: None,
        }
    }

    #[test]
    fn test_extract_chunk_handler() -> CarrierResult<()> {
        let (conn, content_root, _dir) = setup();
        let chunk_store = ChunkStore::new(conn.clone());
        let entity_store = EntityStore::new(conn.clone());
        let job_store = JobStore::new(conn.clone());

        let chunk = crate::tree::types::Chunk {
            id: "chunk_test".to_string(),
            owner_id: "owner_1".to_string(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:test:sender".to_string(),
            source_ref: None,
            timestamp_ms: 1_700_000_000_000,
            time_range_start_ms: 1_700_000_000_000,
            time_range_end_ms: 1_700_000_000_000,
            tags_json: "[]".to_string(),
            content: "Contact alice@example.com for project details".to_string(),
            token_count: 100,
            seq_in_source: 0,
            partial_message: false,
            lifecycle_status: "pending_extraction".to_string(),
            created_at_ms: 1_700_000_000_000,
        };
        chunk_store.upsert_chunks(&[chunk])?;

        let payload = ExtractChunkPayload {
            chunk_id: "chunk_test".to_string(),
        };
        let job = mk_job("owner_1", JobKind::ExtractChunk, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&conn, &content_root, "owner_1", &job)?;
        assert_eq!(result, JobOutcome::Done);

        let updated = chunk_store.get_chunk("owner_1", "chunk_test")?.unwrap();
        assert_eq!(updated.lifecycle_status, "admitted");

        let entities = entity_store.entities_for_node("owner_1", "chunk_test")?;
        assert!(entities.iter().any(|e| e.starts_with("email:")));

        let pending = job_store.count_pending("owner_1", None)?;
        assert!(pending >= 1);

        Ok(())
    }

    #[test]
    fn test_append_buffer_handler() -> CarrierResult<()> {
        let (conn, content_root, _dir) = setup();
        let tree_store = TreeTreeStore::new(conn.clone());
        let chunk_store = ChunkStore::new(conn.clone());

        let tree = tree_store.get_or_create_tree("owner_1", TreeKind::Source, "wechat:test:sender")?;
        let chunk = crate::tree::types::Chunk {
            id: "chunk_ab".to_string(),
            owner_id: "owner_1".to_string(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:test:sender".to_string(),
            source_ref: None,
            timestamp_ms: 1_700_000_000_000,
            time_range_start_ms: 1_700_000_000_000,
            time_range_end_ms: 1_700_000_000_000,
            tags_json: "[]".to_string(),
            content: "test content".to_string(),
            token_count: 100,
            seq_in_source: 0,
            partial_message: false,
            lifecycle_status: "admitted".to_string(),
            created_at_ms: 1_700_000_000_000,
        };
        chunk_store.upsert_chunks(&[chunk])?;

        let payload = AppendBufferPayload {
            node: NodeRef::Leaf { chunk_id: "chunk_ab".to_string() },
            target: AppendTarget::Source { source_id: "wechat:test:sender".to_string() },
        };
        let job = mk_job("owner_1", JobKind::AppendBuffer, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&conn, &content_root, "owner_1", &job)?;
        assert_eq!(result, JobOutcome::Done);

        let seal_engine = BucketSealEngine::new(conn.clone(), content_root.to_path_buf(), Arc::new(InertSummariser));
        let buf = seal_engine.get_or_create_buffer("owner_1", &tree.id, 0)?;
        assert!(buf.item_ids.contains(&"chunk_ab".to_string()));

        Ok(())
    }

    #[test]
    fn test_seal_handler() -> CarrierResult<()> {
        let (conn, content_root, _dir) = setup();
        let tree_store = TreeTreeStore::new(conn.clone());
        let chunk_store = ChunkStore::new(conn.clone());

        let tree = tree_store.get_or_create_tree("owner_1", TreeKind::Source, "wechat:test:sender")?;

        for i in 0..10 {
            let chunk_id = format!("chunk_seal_{i}");
            let chunk = crate::tree::types::Chunk {
                id: chunk_id.clone(),
                owner_id: "owner_1".to_string(),
                agent_id: "agent_1".to_string(),
                source_kind: SourceKind::Chat,
                source_id: "wechat:test:sender".to_string(),
                source_ref: None,
                timestamp_ms: 1_700_000_000_000 + i as i64,
                time_range_start_ms: 1_700_000_000_000,
                time_range_end_ms: 1_700_000_000_000,
                tags_json: "[]".to_string(),
                content: "test content for seal test".to_string(),
                token_count: 6000,
                seq_in_source: i,
                partial_message: false,
                lifecycle_status: "admitted".to_string(),
                created_at_ms: 1_700_000_000_000,
            };
            chunk_store.upsert_chunks(&[chunk])?;
        }

        let seal_engine = BucketSealEngine::new(conn.clone(), content_root.to_path_buf(), Arc::new(InertSummariser));
        for i in 0..10 {
            seal_engine.append_to_buffer("owner_1", &tree.id, 0, &format!("chunk_seal_{i}"), 6000, 1_700_000_000_000)?;
        }

        let payload = SealPayload {
            tree_id: tree.id.clone(),
            level: 0,
            force_now_ms: None,
        };
        let job = mk_job("owner_1", JobKind::Seal, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&conn, &content_root, "owner_1", &job)?;
        assert_eq!(result, JobOutcome::Done);

        let summaries = tree_store.list_summaries("owner_1", &tree.id, Some(1), 100)?;
        assert!(!summaries.is_empty());

        Ok(())
    }

    #[test]
    fn test_digest_daily_handler() -> CarrierResult<()> {
        let (conn, content_root, _dir) = setup();
        let payload = DigestDailyPayload {
            date_iso: "2026-05-16".to_string(),
        };
        let job = mk_job("owner_1", JobKind::DigestDaily, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&conn, &content_root, "owner_1", &job)?;
        assert_eq!(result, JobOutcome::Done);
        Ok(())
    }

    #[test]
    fn test_flush_stale_handler() -> CarrierResult<()> {
        let (conn, content_root, _dir) = setup();
        let tree_store = TreeTreeStore::new(conn.clone());
        let chunk_store = ChunkStore::new(conn.clone());
        let job_store = JobStore::new(conn.clone());

        let tree = tree_store.get_or_create_tree("owner_1", TreeKind::Source, "wechat:stale:sender")?;
        let chunk = crate::tree::types::Chunk {
            id: "chunk_stale".to_string(),
            owner_id: "owner_1".to_string(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:stale:sender".to_string(),
            source_ref: None,
            timestamp_ms: 1_000_000_000_000,
            time_range_start_ms: 1_000_000_000_000,
            time_range_end_ms: 1_000_000_000_000,
            tags_json: "[]".to_string(),
            content: "stale content".to_string(),
            token_count: 100,
            seq_in_source: 0,
            partial_message: false,
            lifecycle_status: "admitted".to_string(),
            created_at_ms: 1_000_000_000_000,
        };
        chunk_store.upsert_chunks(&[chunk])?;

        let seal_engine = BucketSealEngine::new(conn.clone(), content_root.to_path_buf(), Arc::new(InertSummariser));
        seal_engine.append_to_buffer("owner_1", &tree.id, 0, "chunk_stale", 100, 1_000_000_000_000)?;

        let payload = FlushStalePayload { max_age_secs: None };
        let job = mk_job("owner_1", JobKind::FlushStale, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&conn, &content_root, "owner_1", &job)?;
        assert_eq!(result, JobOutcome::Done);

        let pending = job_store.count_pending("owner_1", Some(JobKind::Seal))?;
        assert!(pending >= 1);

        Ok(())
    }

    #[test]
    fn test_extract_missing_chunk_is_done() -> CarrierResult<()> {
        let (conn, content_root, _dir) = setup();
        let payload = ExtractChunkPayload { chunk_id: "nonexistent".to_string() };
        let job = mk_job("owner_1", JobKind::ExtractChunk, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&conn, &content_root, "owner_1", &job)?;
        assert_eq!(result, JobOutcome::Done);
        Ok(())
    }
}
