//! Per-`JobKind` handler implementations dispatched by the worker pool.
//!
//! Ported from `memory::tree::jobs::handlers` - same dispatch + per-kind logic,
//! stores swapped to the PG async stores and every handler is `async`. Pure
//! helpers (`extract_entities`) and the ported `routing`/`digest` modules are
//! reused from this crate / the memory crate.

use std::path::Path;
use std::sync::Arc;

use deadpool_postgres::Pool;
use memory::tree::entity_store::EntityIndexEntry;
use memory::tree::extract::extract_entities;
use memory::tree::summariser::inert::InertSummariser;
use memory::tree::types::{
    AppendBufferPayload, AppendTarget, DigestDailyPayload, ExtractChunkPayload, FlushStalePayload,
    Job, JobKind, NodeRef, NewJob, SealPayload, TopicRoutePayload, DEFAULT_FLUSH_AGE_SECS,
};
use types::error::{CarrierError, CarrierResult};
use types::memory_tree::TreeKind;

use crate::bucket_seal::BucketSealEngine;
use crate::digest::{end_of_day_digest, DigestOutcome};
use crate::pg::chunk_store::ChunkStore;
use crate::pg::entity_store::EntityStore;
use crate::pg::job_store::JobStore;
use crate::pg::tree_store::TreeStore;
use crate::routing::route_leaf_to_topic_trees;

/// Outcome of a successful handler run.
#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    Done,
    Defer { until_ms: i64, reason: String },
}

/// Dispatch a claimed job to the matching per-kind handler.
pub async fn handle_job(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    match job.kind {
        JobKind::ExtractChunk => handle_extract(pool, content_root, owner_id, job).await,
        JobKind::AppendBuffer => handle_append_buffer(pool, content_root, owner_id, job).await,
        JobKind::Seal => handle_seal(pool, content_root, owner_id, job).await,
        JobKind::TopicRoute => handle_topic_route(pool, content_root, owner_id, job).await,
        JobKind::DigestDaily => handle_digest_daily(pool, content_root, owner_id, job).await,
        JobKind::FlushStale => handle_flush_stale(pool, content_root, owner_id, job).await,
    }
}

async fn handle_extract(
    pool: &Pool,
    _content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: ExtractChunkPayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| CarrierError::Internal(format!("parse ExtractChunk payload: {e}")))?;

    let chunk_store = ChunkStore::new(pool.clone());
    let entity_store = EntityStore::new(pool.clone());
    let job_store = JobStore::new(pool.clone());

    let Some(chunk) = chunk_store.get_chunk(owner_id, None, &payload.chunk_id).await? else {
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
            user_id: chunk.user_id.as_str(),
        };
        entity_store.upsert_entity_index(owner_id, &entry).await?;
    }

    // Mark as admitted
    chunk_store
        .update_lifecycle(owner_id, &chunk.id, "admitted")
        .await?;

    // Enqueue follow-up: AppendBuffer to source tree
    let append_payload = AppendBufferPayload {
        node: NodeRef::Leaf {
            chunk_id: chunk.id.clone(),
        },
        target: AppendTarget::Source {
            source_id: chunk.source_id.clone(),
        },
    };
    let dedupe_key = format!("append:source:{}:leaf:{}", chunk.source_id, chunk.id);
    let new_job = NewJob {
        owner_id: owner_id.to_string(),
        kind: JobKind::AppendBuffer,
        payload_json: serde_json::to_string(&append_payload)
            .map_err(|e| CarrierError::Internal(e.to_string()))?,
        dedupe_key: Some(dedupe_key),
        available_at_ms: None,
        max_attempts: None,
    };
    job_store.enqueue(&new_job).await?;

    // Enqueue follow-up: TopicRoute
    if !entity_ids.is_empty() {
        let route_payload = TopicRoutePayload {
            node: NodeRef::Leaf {
                chunk_id: chunk.id.clone(),
            },
        };
        let dedupe_key = format!("topic_route:leaf:{}", chunk.id);
        let new_job = NewJob {
            owner_id: owner_id.to_string(),
            kind: JobKind::TopicRoute,
            payload_json: serde_json::to_string(&route_payload)
                .map_err(|e| CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job).await?;
    }

    Ok(JobOutcome::Done)
}

async fn handle_append_buffer(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: AppendBufferPayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| CarrierError::Internal(format!("parse AppendBuffer payload: {e}")))?;

    let tree_store = TreeStore::new(pool.clone());
    let chunk_store = ChunkStore::new(pool.clone());
    let job_store = JobStore::new(pool.clone());

    // Resolve the node first (we need its user_id to create the source tree with
    // the correct per-user isolation).
    let (item_id, token_count, timestamp_ms, node_user_id) = match &payload.node {
        NodeRef::Leaf { chunk_id } => {
            let Some(chunk) = chunk_store.get_chunk(owner_id, None, chunk_id).await? else {
                tracing::warn!(
                    "[tree_jobs] append_buffer chunk missing chunk_id={chunk_id}"
                );
                return Ok(JobOutcome::Done);
            };
            (chunk.id.clone(), chunk.token_count, chunk.timestamp_ms, chunk.user_id.clone())
        }
        NodeRef::Summary { summary_id } => {
            let Some(summary) = tree_store.get_summary(owner_id, None, summary_id).await? else {
                tracing::warn!(
                    "[tree_jobs] append_buffer summary missing summary_id={summary_id}"
                );
                return Ok(JobOutcome::Done);
            };
            (
                summary.id.clone(),
                summary.token_count,
                summary.time_range_start_ms,
                summary.user_id.clone(),
            )
        }
    };

    // Resolve the tree for the target.
    // Source trees inherit the appended node's user_id (per-user isolation);
    // topic trees are owner-shared (user_id = "") and already exist.
    let tree = match &payload.target {
        AppendTarget::Source { source_id } => {
            tree_store
                .get_or_create_tree(owner_id, &node_user_id, TreeKind::Source, source_id)
                .await?
        }
        AppendTarget::Topic { tree_id } => match tree_store.get_tree(owner_id, None, tree_id).await? {
            Some(t) => t,
            None => {
                tracing::warn!(
                    "[tree_jobs] append_buffer topic tree missing tree_id={tree_id}"
                );
                return Ok(JobOutcome::Done);
            }
        },
    };

    let seal_engine = BucketSealEngine::new(
        pool.clone(),
        content_root.to_path_buf(),
        Arc::new(InertSummariser),
    );

    // Append to L0 buffer
    seal_engine
        .append_to_buffer(owner_id, &tree.id, 0, &item_id, token_count as i64, timestamp_ms)
        .await?;

    // Check if seal should happen
    let buf = seal_engine.get_or_create_buffer(owner_id, &tree.id, 0).await?;
    if crate::bucket_seal::should_seal(&buf) {
        let seal_payload = SealPayload {
            tree_id: tree.id.clone(),
            level: 0,
            force_now_ms: None,
        };
        let dedupe_key = format!("seal:{}:0", tree.id);
        let new_job = NewJob {
            owner_id: owner_id.to_string(),
            kind: JobKind::Seal,
            payload_json: serde_json::to_string(&seal_payload)
                .map_err(|e| CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job).await?;
    }

    // Update lifecycle for source-target leaf chunks
    if matches!(payload.target, AppendTarget::Source { .. }) {
        if let NodeRef::Leaf { chunk_id } = &payload.node {
            chunk_store
                .update_lifecycle(owner_id, chunk_id, "buffered")
                .await?;
        }
    }

    Ok(JobOutcome::Done)
}

async fn handle_seal(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: SealPayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| CarrierError::Internal(format!("parse Seal payload: {e}")))?;

    let tree_store = TreeStore::new(pool.clone());
    let job_store = JobStore::new(pool.clone());

    let Some(tree) = tree_store.get_tree(owner_id, None, &payload.tree_id).await? else {
        tracing::warn!(
            "[tree_jobs] seal tree missing tree_id={}",
            payload.tree_id
        );
        return Ok(JobOutcome::Done);
    };

    let seal_engine = BucketSealEngine::new(
        pool.clone(),
        content_root.to_path_buf(),
        Arc::new(InertSummariser),
    );

    let forced = payload.force_now_ms.is_some();
    let sealed_ids = seal_engine
        .cascade_seals(owner_id, &tree, payload.level, forced)
        .await?;

    // For source trees, enqueue TopicRoute for each new summary
    if tree.kind == TreeKind::Source {
        for summary_id in &sealed_ids {
            let route_payload = TopicRoutePayload {
                node: NodeRef::Summary {
                    summary_id: summary_id.clone(),
                },
            };
            let dedupe_key = format!("topic_route:summary:{summary_id}");
            let new_job = NewJob {
                owner_id: owner_id.to_string(),
                kind: JobKind::TopicRoute,
                payload_json: serde_json::to_string(&route_payload)
                    .map_err(|e| CarrierError::Internal(e.to_string()))?,
                dedupe_key: Some(dedupe_key),
                available_at_ms: None,
                max_attempts: None,
            };
            job_store.enqueue(&new_job).await?;
        }
    }

    Ok(JobOutcome::Done)
}

async fn handle_topic_route(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: TopicRoutePayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| CarrierError::Internal(format!("parse TopicRoute payload: {e}")))?;

    let tree_store = TreeStore::new(pool.clone());
    let entity_store = EntityStore::new(pool.clone());

    // Get entity IDs for the node
    let node_id = match &payload.node {
        NodeRef::Leaf { chunk_id } => chunk_id.clone(),
        NodeRef::Summary { summary_id } => summary_id.clone(),
    };

    let entity_ids = entity_store.entities_for_node(owner_id, None, &node_id).await?;
    if entity_ids.is_empty() {
        return Ok(JobOutcome::Done);
    }

    // Get token count and timestamp for routing
    let (token_count, timestamp_ms) = match &payload.node {
        NodeRef::Leaf { chunk_id } => {
            let chunk_store = ChunkStore::new(pool.clone());
            if let Some(chunk) = chunk_store.get_chunk(owner_id, None, chunk_id).await? {
                (chunk.token_count, chunk.timestamp_ms)
            } else {
                return Ok(JobOutcome::Done);
            }
        }
        NodeRef::Summary { summary_id } => {
            if let Some(summary) = tree_store.get_summary(owner_id, None, summary_id).await? {
                (summary.token_count, summary.time_range_start_ms)
            } else {
                return Ok(JobOutcome::Done);
            }
        }
    };

    route_leaf_to_topic_trees(
        pool,
        content_root,
        owner_id,
        &node_id,
        token_count,
        timestamp_ms,
        &entity_ids,
    )
    .await?;

    Ok(JobOutcome::Done)
}

async fn handle_digest_daily(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let date = serde_json::from_str::<DigestDailyPayload>(&job.payload_json)
        .ok()
        .and_then(|p| chrono::NaiveDate::parse_from_str(&p.date_iso, "%Y-%m-%d").ok())
        .unwrap_or_else(|| chrono::Utc::now().date_naive() - chrono::Duration::days(1));

    match end_of_day_digest(pool, content_root, owner_id, date, &InertSummariser).await? {
        DigestOutcome::Emitted { daily_id, .. } => {
            tracing::info!(
                date = %date,
                daily_id = %daily_id,
                "[tree_jobs] emitted digest"
            );
        }
        DigestOutcome::EmptyDay => {}
        DigestOutcome::Skipped { existing_id } => {
            tracing::info!(
                date = %date,
                existing_id = %existing_id,
                "[tree_jobs] digest skipped (already have this day)"
            );
        }
    }
    Ok(JobOutcome::Done)
}

async fn handle_flush_stale(
    pool: &Pool,
    _content_root: &Path,
    owner_id: &str,
    job: &Job,
) -> CarrierResult<JobOutcome> {
    let payload: FlushStalePayload = serde_json::from_str(&job.payload_json)
        .map_err(|e| CarrierError::Internal(format!("parse FlushStale payload: {e}")))?;

    let age_secs = payload.max_age_secs.unwrap_or(DEFAULT_FLUSH_AGE_SECS);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cutoff_ms = now_ms - (age_secs * 1000);

    let tree_store = TreeStore::new(pool.clone());
    let job_store = JobStore::new(pool.clone());

    // Find buffers with items older than cutoff
    let stale_buffers = tree_store.list_stale_buffers(owner_id, cutoff_ms).await?;

    for buf in stale_buffers {
        let seal_payload = SealPayload {
            tree_id: buf.tree_id.clone(),
            level: buf.level,
            force_now_ms: Some(now_ms),
        };
        let dedupe_key = format!("seal:{}:{}", buf.tree_id, buf.level);
        let new_job = NewJob {
            owner_id: owner_id.to_string(),
            kind: JobKind::Seal,
            payload_json: serde_json::to_string(&seal_payload)
                .map_err(|e| CarrierError::Internal(e.to_string()))?,
            dedupe_key: Some(dedupe_key),
            available_at_ms: None,
            max_attempts: None,
        };
        job_store.enqueue(&new_job).await?;
    }

    Ok(JobOutcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;
    use memory::tree::types::{Chunk, JobStatus, SourceKind};
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn setup() -> Option<(Pool, PathBuf, TempDir)> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        let dir = TempDir::new().ok()?;
        Some((pool, dir.path().to_path_buf(), dir))
    }

    fn mk_job(owner_id: &str, kind: JobKind, payload_json: &str) -> Job {
        let now_ms = chrono::Utc::now().timestamp_millis();
        Job {
            id: "test-job-id".to_string(),
            owner_id: owner_id.to_string(),
            kind,
            payload_json: payload_json.to_string(),
            dedupe_key: None,
            status: JobStatus::Running,
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

    fn mk_chunk(owner: &str, id: &str, content: &str, lifecycle: &str) -> Chunk {
        Chunk {
            id: id.to_string(),
            owner_id: owner.to_string(),
            user_id: String::new(),
            agent_id: "agent_1".to_string(),
            source_kind: SourceKind::Chat,
            source_id: "wechat:test:sender".to_string(),
            source_ref: None,
            timestamp_ms: 1_700_000_000_000,
            time_range_start_ms: 1_700_000_000_000,
            time_range_end_ms: 1_700_000_000_000,
            tags_json: "[]".to_string(),
            content: content.to_string(),
            token_count: 100,
            seq_in_source: 0,
            partial_message: false,
            lifecycle_status: lifecycle.to_string(),
            created_at_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn extract_chunk_handler() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let chunk_store = ChunkStore::new(pool.clone());
        let entity_store = EntityStore::new(pool.clone());
        let job_store = JobStore::new(pool.clone());

        chunk_store
            .upsert_chunks(&[mk_chunk(
                "owner_1",
                "chunk_test",
                "Contact alice@example.com for project details",
                "pending_extraction",
            )])
            .await
            .unwrap();

        let payload = ExtractChunkPayload { chunk_id: "chunk_test".to_string() };
        let job = mk_job("owner_1", JobKind::ExtractChunk, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&pool, &content_root, "owner_1", &job).await.unwrap();
        assert_eq!(result, JobOutcome::Done);

        let updated = chunk_store.get_chunk("owner_1", None, "chunk_test").await.unwrap().unwrap();
        assert_eq!(updated.lifecycle_status, "admitted");

        let entities = entity_store
            .entities_for_node("owner_1", None, "chunk_test")
            .await
            .unwrap();
        assert!(entities.iter().any(|e| e.starts_with("email:")));

        let pending = job_store.count_pending("owner_1", None).await.unwrap();
        assert!(pending >= 1);
    }

    #[tokio::test]
    async fn append_buffer_handler() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree_store = TreeStore::new(pool.clone());
        let chunk_store = ChunkStore::new(pool.clone());

        let tree = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:test:sender")
            .await
            .unwrap();
        chunk_store
            .upsert_chunks(&[mk_chunk("owner_1", "chunk_ab", "test content", "admitted")])
            .await
            .unwrap();

        let payload = AppendBufferPayload {
            node: NodeRef::Leaf { chunk_id: "chunk_ab".to_string() },
            target: AppendTarget::Source { source_id: "wechat:test:sender".to_string() },
        };
        let job = mk_job("owner_1", JobKind::AppendBuffer, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&pool, &content_root, "owner_1", &job).await.unwrap();
        assert_eq!(result, JobOutcome::Done);

        let seal_engine = BucketSealEngine::new(pool.clone(), content_root.to_path_buf(), Arc::new(InertSummariser));
        let buf = seal_engine.get_or_create_buffer("owner_1", &tree.id, 0).await.unwrap();
        assert!(buf.item_ids.contains(&"chunk_ab".to_string()));
    }

    #[tokio::test]
    async fn seal_handler() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree_store = TreeStore::new(pool.clone());
        let chunk_store = ChunkStore::new(pool.clone());

        let tree = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:test:sender")
            .await
            .unwrap();

        for i in 0..10 {
            let chunk_id = format!("chunk_seal_{i}");
            let mut c = mk_chunk("owner_1", &chunk_id, "test content for seal test", "admitted");
            c.token_count = 6000;
            c.seq_in_source = i;
            chunk_store.upsert_chunks(&[c]).await.unwrap();
        }

        let seal_engine = BucketSealEngine::new(pool.clone(), content_root.to_path_buf(), Arc::new(InertSummariser));
        for i in 0..10 {
            seal_engine
                .append_to_buffer("owner_1", &tree.id, 0, &format!("chunk_seal_{i}"), 6000, 1_700_000_000_000)
                .await
                .unwrap();
        }

        let payload = SealPayload { tree_id: tree.id.clone(), level: 0, force_now_ms: None };
        let job = mk_job("owner_1", JobKind::Seal, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&pool, &content_root, "owner_1", &job).await.unwrap();
        assert_eq!(result, JobOutcome::Done);

        let summaries = tree_store
            .list_summaries("owner_1", None, &tree.id, Some(1), 100)
            .await
            .unwrap();
        assert!(!summaries.is_empty());
    }

    #[tokio::test]
    async fn digest_daily_handler() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let payload = DigestDailyPayload { date_iso: "2026-05-16".to_string() };
        let job = mk_job("owner_1", JobKind::DigestDaily, &serde_json::to_string(&payload).unwrap());
        let result = handle_job(&pool, &content_root, "owner_1", &job).await.unwrap();
        assert_eq!(result, JobOutcome::Done);
    }

    #[tokio::test]
    async fn flush_stale_handler() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree_store = TreeStore::new(pool.clone());
        let chunk_store = ChunkStore::new(pool.clone());
        let job_store = JobStore::new(pool.clone());

        let tree = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:stale:sender")
            .await
            .unwrap();
        let mut c = mk_chunk("owner_1", "chunk_stale", "stale content", "admitted");
        c.timestamp_ms = 1_000_000_000_000;
        c.time_range_start_ms = 1_000_000_000_000;
        c.time_range_end_ms = 1_000_000_000_000;
        c.created_at_ms = 1_000_000_000_000;
        chunk_store.upsert_chunks(&[c]).await.unwrap();

        let seal_engine = BucketSealEngine::new(pool.clone(), content_root.to_path_buf(), Arc::new(InertSummariser));
        seal_engine
            .append_to_buffer("owner_1", &tree.id, 0, "chunk_stale", 100, 1_000_000_000_000)
            .await
            .unwrap();

        let payload = FlushStalePayload::default();
        let job = mk_job("owner_1", JobKind::FlushStale, &serde_json::to_string(&payload).unwrap());

        let result = handle_job(&pool, &content_root, "owner_1", &job).await.unwrap();
        assert_eq!(result, JobOutcome::Done);

        let pending = job_store.count_pending("owner_1", Some(JobKind::Seal)).await.unwrap();
        assert!(pending >= 1);
    }

    #[tokio::test]
    async fn extract_missing_chunk_is_done() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let payload = ExtractChunkPayload { chunk_id: "nonexistent".to_string() };
        let job = mk_job("owner_1", JobKind::ExtractChunk, &serde_json::to_string(&payload).unwrap());
        let result = handle_job(&pool, &content_root, "owner_1", &job).await.unwrap();
        assert_eq!(result, JobOutcome::Done);
    }
}
