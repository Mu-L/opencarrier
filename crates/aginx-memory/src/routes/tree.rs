//! `/tree/*` handlers - back the 8 async tree trait methods on `HttpMemoryHandle`.
//!
//! Each handler deserializes the owned query type, maps it to the retrieval/
//! ingest call (matching `MemorySubstrate::*_async`'s translation), and returns
//! JSON in the shape `HttpMemoryHandle::parse_response` expects.

use axum::extract::State;
use axum::Json;
use memory::tree::types::SourceKind;
use serde::Deserialize;
use types::memory_tree::{
    DrillDownQueryOwned, EntitySearchOwned, FetchLeavesQueryOwned, GlobalQueryOwned,
    IngestRequest, IngestResult, QueryResponse, SourceQueryOwned, TopicQueryOwned, TreeSummary,
};
use types::memory_tree::{EntityMatch, TreeKind};

use crate::ingest::IngestPipeline;
use crate::pg::entity_store::EntityStore;
use crate::pg::tree_store::TreeStore;
use crate::retrieval;
use crate::server::{err_resp, AppState};
use types::error::CarrierResult;

fn to_resp<T>(r: CarrierResult<T>) -> Result<T, (axum::http::StatusCode, String)> {
    r.map_err(err_resp)
}

fn parse_source_kind(s: &str) -> Option<SourceKind> {
    match s {
        "chat" => Some(SourceKind::Chat),
        "email" => Some(SourceKind::Email),
        "document" => Some(SourceKind::Document),
        _ => None,
    }
}

pub async fn tree_ingest(
    State(state): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> Result<Json<IngestResult>, (axum::http::StatusCode, String)> {
    let pipeline = IngestPipeline::new(state.pool.clone(), state.content_root.clone());
    let result = to_resp(pipeline.ingest(&req).await)?;
    // Wake idle workers so newly-enqueued ExtractChunk jobs are picked up
    // promptly (no-op when worker_count == 0).
    state.worker_pool.wake();
    Ok(Json(result))
}

pub async fn query_source(
    State(state): State<AppState>,
    Json(req): Json<SourceQueryOwned>,
) -> Result<Json<QueryResponse>, (axum::http::StatusCode, String)> {
    let source_kind = req.source_kind.as_deref().and_then(parse_source_kind);
    let resp = retrieval::source::query_source(
        &state.pool,
        &req.owner_id,
        req.user_id.as_deref(),
        req.source_id.as_deref(),
        source_kind,
        req.time_window_days,
        req.limit,
    )
    .await;
    to_resp(resp).map(Json)
}

pub async fn query_global(
    State(state): State<AppState>,
    Json(req): Json<GlobalQueryOwned>,
) -> Result<Json<QueryResponse>, (axum::http::StatusCode, String)> {
    let resp =
        retrieval::global::query_global(&state.pool, &req.owner_id, req.time_window_days, req.limit)
            .await;
    to_resp(resp).map(Json)
}

pub async fn query_topic(
    State(state): State<AppState>,
    Json(req): Json<TopicQueryOwned>,
) -> Result<Json<QueryResponse>, (axum::http::StatusCode, String)> {
    let resp = retrieval::topic::query_topic(
        &state.pool,
        &req.owner_id,
        req.user_id.as_deref(),
        &req.entity_id,
        req.time_window_days,
        req.limit,
    )
    .await;
    to_resp(resp).map(Json)
}

pub async fn search_entities(
    State(state): State<AppState>,
    Json(req): Json<EntitySearchOwned>,
) -> Result<Json<Vec<EntityMatch>>, (axum::http::StatusCode, String)> {
    let kind = req.kind.as_deref().map(EntityStore::parse_entity_kind);
    let resp = retrieval::search::search_entities(
        &state.pool,
        &req.owner_id,
        req.user_id.as_deref(),
        &req.query,
        kind,
        req.limit,
    )
    .await;
    to_resp(resp).map(Json)
}

pub async fn drill_down(
    State(state): State<AppState>,
    Json(req): Json<DrillDownQueryOwned>,
) -> Result<Json<QueryResponse>, (axum::http::StatusCode, String)> {
    let max_depth = req.max_depth.clamp(1, 3);
    let limit = req.limit;
    let hits = to_resp(
        retrieval::drill_down::drill_down(
            &state.pool,
            &req.owner_id,
            req.user_id.as_deref(),
            &req.node_id,
            max_depth,
            Some(limit),
        )
        .await,
    )?;
    let total = hits.len();
    let truncated = total > limit;
    Ok(Json(QueryResponse {
        hits,
        total,
        truncated,
    }))
}

pub async fn fetch_leaves(
    State(state): State<AppState>,
    Json(req): Json<FetchLeavesQueryOwned>,
) -> Result<Json<QueryResponse>, (axum::http::StatusCode, String)> {
    let resp = retrieval::fetch::fetch_leaves(
        &state.pool,
        &req.owner_id,
        req.user_id.as_deref(),
        &req.chunk_ids,
        req.limit,
    )
    .await;
    to_resp(resp).map(Json)
}

#[derive(Deserialize)]
pub struct ListSourcesReq {
    pub owner_id: String,
    pub source_kind: Option<String>,
    pub limit: usize,
}

pub async fn list_sources(
    State(state): State<AppState>,
    Json(req): Json<ListSourcesReq>,
) -> Result<Json<Vec<TreeSummary>>, (axum::http::StatusCode, String)> {
    let tree_store = TreeStore::new(state.pool.clone());
    // Owner-level listing (no user filter) - metadata only.
    let mut trees = to_resp(
        tree_store
            .list_trees(&req.owner_id, None, Some(TreeKind::Source), req.limit)
            .await,
    )?;
    if let Some(ref sk) = req.source_kind {
        trees.retain(|t| t.scope.starts_with(&format!("{sk}:")));
    }
    Ok(Json(trees))
}
