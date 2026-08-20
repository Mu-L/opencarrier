//! aginxMemory HTTP server: axum router + AppState + serve loop.
//!
//! Exposes the 12 endpoints that opencarrier's `HttpMemoryHandle` calls:
//! `/health`, `/kv/{set,get,list,delete}`, `/tree/ingest`,
//! `/tree/{query_source,query_global,query_topic,search_entities,drill_down,
//! fetch_leaves,list_sources}`. Handlers live in [`crate::routes`].

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use deadpool_postgres::Pool;
use types::error::CarrierError;

use crate::jobs::worker::TreeWorkerPool;
use crate::routes;

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    pub content_root: PathBuf,
    pub worker_pool: Arc<TreeWorkerPool>,
}

/// Map a `CarrierError` to an axum error response (500 + message). The opencarrier
/// `HttpMemoryHandle` treats any non-2xx status as a network error, so the body
/// text surfaces there.
pub(crate) fn err_resp(e: CarrierError) -> (axum::http::StatusCode, String) {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Build the aginxMemory router over the shared [`AppState`].
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        .route("/kv/set", axum::routing::post(routes::kv::kv_set))
        .route("/kv/get", axum::routing::post(routes::kv::kv_get))
        .route("/kv/list", axum::routing::post(routes::kv::kv_list))
        .route("/kv/delete", axum::routing::post(routes::kv::kv_delete))
        .route(
            "/tree/ingest",
            axum::routing::post(routes::tree::tree_ingest),
        )
        .route(
            "/tree/query_source",
            axum::routing::post(routes::tree::query_source),
        )
        .route(
            "/tree/query_global",
            axum::routing::post(routes::tree::query_global),
        )
        .route(
            "/tree/query_topic",
            axum::routing::post(routes::tree::query_topic),
        )
        .route(
            "/tree/search_entities",
            axum::routing::post(routes::tree::search_entities),
        )
        .route(
            "/tree/drill_down",
            axum::routing::post(routes::tree::drill_down),
        )
        .route(
            "/tree/fetch_leaves",
            axum::routing::post(routes::tree::fetch_leaves),
        )
        .route(
            "/tree/list_sources",
            axum::routing::post(routes::tree::list_sources),
        )
        .with_state(state)
}
