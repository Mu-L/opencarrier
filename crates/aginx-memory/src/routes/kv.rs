//! `/kv/*` handlers - back the 4 sync kv trait methods on `HttpMemoryHandle`.
//!
//! Request bodies match what `HttpMemoryHandle` POSTs (flat JSON with
//! `agent_id`/`owner_id`/`user_id`/`key`/`value`). Responses are JSON-serialized
//! to the shapes `HttpMemoryHandle::parse_response` deserializes (`()` for
//! set/delete serializes to `null`, which deserializes back to `()`).

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::pg::kv_store::KvStore;
use crate::server::{err_resp, AppState};
use types::error::CarrierResult;

#[derive(Deserialize)]
pub struct KvSetReq {
    pub agent_id: String,
    pub owner_id: String,
    pub user_id: String,
    pub key: String,
    pub value: Value,
}

#[derive(Deserialize)]
pub struct KvKeyReq {
    pub agent_id: String,
    pub owner_id: String,
    pub user_id: String,
    pub key: String,
}

#[derive(Deserialize)]
pub struct KvScopeReq {
    pub agent_id: String,
    pub owner_id: String,
    pub user_id: String,
}

pub async fn kv_set(
    State(state): State<AppState>,
    Json(req): Json<KvSetReq>,
) -> Result<Json<()>, (axum::http::StatusCode, String)> {
    let store = KvStore::new(state.pool.clone());
    to_resp(store.set(&req.agent_id, &req.owner_id, &req.user_id, &req.key, req.value).await).map(Json)
}

pub async fn kv_get(
    State(state): State<AppState>,
    Json(req): Json<KvKeyReq>,
) -> Result<Json<Option<Value>>, (axum::http::StatusCode, String)> {
    let store = KvStore::new(state.pool.clone());
    to_resp(store.get(&req.agent_id, &req.owner_id, &req.user_id, &req.key).await).map(Json)
}

pub async fn kv_list(
    State(state): State<AppState>,
    Json(req): Json<KvScopeReq>,
) -> Result<Json<Vec<(String, Value)>>, (axum::http::StatusCode, String)> {
    let store = KvStore::new(state.pool.clone());
    to_resp(store.list_kv(&req.agent_id, &req.owner_id, &req.user_id).await).map(Json)
}

pub async fn kv_delete(
    State(state): State<AppState>,
    Json(req): Json<KvKeyReq>,
) -> Result<Json<()>, (axum::http::StatusCode, String)> {
    let store = KvStore::new(state.pool.clone());
    to_resp(store.delete(&req.agent_id, &req.owner_id, &req.user_id, &req.key).await).map(Json)
}

/// Flatten a `CarrierResult<T>` into the axum `Result<Json<T>, _>` shape.
fn to_resp<T>(r: CarrierResult<T>) -> Result<T, (axum::http::StatusCode, String)> {
    r.map_err(err_resp)
}
