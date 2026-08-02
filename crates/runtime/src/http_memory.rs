//! `HttpMemoryHandle` - delegates kv+tree memory operations to the external
//! aginxMemory service over HTTP, while analytics stay in-process.
//!
//! This is the opencarrier-side counterpart to the `aginxMemory` daemon
//! (`crates/aginx-memory`). When `AGINXMEMORY_URL` is set, the kernel builds
//! this handle instead of `MemorySubstrateHandle` at the 6 injection points, so
//! all kv/tree calls (tools + agent_loop end_turn + compaction) route to the
//! external PG-backed service. Sessions and other runtime state stay in-process
//! SQLite, so the 4 analytics methods (which read sessions/usage) delegate to
//! the local `MemorySubstrate` fallback.
//!
//! The `MemoryHandle` trait declares kv methods as **sync** but HTTP is async.
//! opencarrier runs a multi-thread tokio runtime, so sync kv methods bridge via
//! `tokio::task::block_in_place` + `Handle::current().block_on`. If this ever
//! panics on a non-worker thread, the fallback is to flip the trait's kv methods
//! to async (cascading change through tools/kv.rs + knowledge.rs callers).

use std::sync::Arc;

use async_trait::async_trait;
use memory::MemorySubstrate;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::runtime::Handle;
use types::error::{CarrierError, CarrierResult};
use types::memory_tree::{
    DrillDownQuery, DrillDownQueryOwned, EntityMatch, EntitySearch, EntitySearchOwned,
    FetchLeavesQuery, FetchLeavesQueryOwned, GlobalQuery, GlobalQueryOwned, IngestRequest,
    IngestResult, QueryResponse, SourceQuery, SourceQueryOwned, TopicQuery, TopicQueryOwned,
    TreeSummary,
};

use crate::memory_handle::MemoryHandle;

/// Default timeout for aginxMemory HTTP calls (seconds).
const AGINX_MEMORY_TIMEOUT_SECS: u64 = 30;

/// Read the aginxMemory URL from the `AGINXMEMORY_URL` env var.
///
/// Returns `None` if unset/empty -> memory stays in-process via
/// `MemorySubstrateHandle` (the default, and the migration-period fallback).
/// The kernel's `make_memory_handle` factory branches on this at the 6
/// injection points + 4 direct call sites.
pub fn aginx_memory_url_opt() -> Option<String> {
    // types::env::get_env (not std::env::var): ~/.opencarrier/.env is loaded into
    // the in-process ENV_OVERRIDES map by load_dotenv, which never calls
    // std::env::set_var. std::env::var would silently miss .env values and
    // make_memory_handle would fall back to in-process SQLite with no error.
    types::env::get_env("AGINXMEMORY_URL").filter(|s| !s.is_empty())
}

/// HTTP-backed `MemoryHandle`. kv+tree go to aginxMemory; analytics stay local.
pub struct HttpMemoryHandle {
    client: Client,
    base_url: String,
    /// In-process substrate for analytics (sessions/usage are runtime state,
    /// not externalized) and as the migration-period fallback substrate.
    fallback: Arc<MemorySubstrate>,
}

impl HttpMemoryHandle {
    pub fn new(base_url: String, fallback: Arc<MemorySubstrate>) -> CarrierResult<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(AGINX_MEMORY_TIMEOUT_SECS))
            .build()
            .map_err(|e| CarrierError::Network(format!("aginxMemory HTTP client build: {e}")))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            fallback,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    /// Bridge a sync trait method to an async HTTP call. Requires a multi-thread
    /// tokio runtime (opencarrier's default).
    fn block_http<F, T>(&self, fut: F) -> CarrierResult<T>
    where
        F: std::future::Future<Output = CarrierResult<T>>,
    {
        tokio::task::block_in_place(|| Handle::current().block_on(fut))
    }

    /// POST a JSON body and deserialize the response.
    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> CarrierResult<T> {
        let resp = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .map_err(|e| CarrierError::Network(format!("aginxMemory {path} request: {e}")))?;
        Self::parse_response(path, resp).await
    }

    async fn parse_response<T: DeserializeOwned>(
        path: &str,
        resp: reqwest::Response,
    ) -> CarrierResult<T> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(CarrierError::Network(format!(
                "aginxMemory {path} error ({status}): {text}"
            )));
        }
        resp.json::<T>()
            .await
            .map_err(|e| CarrierError::Serialization(format!("aginxMemory {path} response: {e}")))
    }
}

#[async_trait]
impl MemoryHandle for HttpMemoryHandle {
    // ── kv (sync trait -> async HTTP via block_in_place) ───────────────────

    fn kv_set(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: Value,
    ) -> CarrierResult<()> {
        let body = json!({
            "agent_id": agent_id,
            "owner_id": owner_id,
            "user_id": user_id,
            "key": key,
            "value": value,
        });
        let client = self.client.clone();
        let url = self.url("kv/set");
        self.block_http(async move {
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| CarrierError::Network(format!("aginxMemory kv/set request: {e}")))?;
            Self::parse_response::<()>("kv/set", resp).await
        })
    }

    fn kv_get(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<Option<Value>> {
        let body = json!({
            "agent_id": agent_id,
            "owner_id": owner_id,
            "user_id": user_id,
            "key": key,
        });
        let client = self.client.clone();
        let url = self.url("kv/get");
        self.block_http(async move {
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| CarrierError::Network(format!("aginxMemory kv/get request: {e}")))?;
            Self::parse_response::<Option<Value>>("kv/get", resp).await
        })
    }

    fn kv_list(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> CarrierResult<Vec<(String, Value)>> {
        let body = json!({
            "agent_id": agent_id,
            "owner_id": owner_id,
            "user_id": user_id,
        });
        let client = self.client.clone();
        let url = self.url("kv/list");
        self.block_http(async move {
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| CarrierError::Network(format!("aginxMemory kv/list request: {e}")))?;
            Self::parse_response::<Vec<(String, Value)>>("kv/list", resp).await
        })
    }

    fn kv_delete(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<()> {
        let body = json!({
            "agent_id": agent_id,
            "owner_id": owner_id,
            "user_id": user_id,
            "key": key,
        });
        let client = self.client.clone();
        let url = self.url("kv/delete");
        self.block_http(async move {
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| CarrierError::Network(format!("aginxMemory kv/delete request: {e}")))?;
            Self::parse_response::<()>("kv/delete", resp).await
        })
    }

    // ── tree (async -> HTTP) ───────────────────────────────────────────────

    async fn tree_ingest(&self, req: IngestRequest) -> CarrierResult<IngestResult> {
        self.post_json("tree/ingest", &req).await
    }

    async fn tree_query_source(&self, req: SourceQuery<'_>) -> CarrierResult<QueryResponse> {
        let owned = SourceQueryOwned::from(req);
        self.post_json("tree/query_source", &owned).await
    }

    async fn tree_query_global(&self, req: GlobalQuery<'_>) -> CarrierResult<QueryResponse> {
        let owned = GlobalQueryOwned::from(req);
        self.post_json("tree/query_global", &owned).await
    }

    async fn tree_query_topic(&self, req: TopicQuery<'_>) -> CarrierResult<QueryResponse> {
        let owned = TopicQueryOwned::from(req);
        self.post_json("tree/query_topic", &owned).await
    }

    async fn tree_search_entities(
        &self,
        req: EntitySearch<'_>,
    ) -> CarrierResult<Vec<EntityMatch>> {
        let owned = EntitySearchOwned::from(req);
        self.post_json("tree/search_entities", &owned).await
    }

    async fn tree_drill_down(&self, req: DrillDownQuery<'_>) -> CarrierResult<QueryResponse> {
        let owned = DrillDownQueryOwned::from(req);
        self.post_json("tree/drill_down", &owned).await
    }

    async fn tree_fetch_leaves(&self, req: FetchLeavesQuery<'_>) -> CarrierResult<QueryResponse> {
        let owned = FetchLeavesQueryOwned::from(req);
        self.post_json("tree/fetch_leaves", &owned).await
    }

    async fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> CarrierResult<Vec<TreeSummary>> {
        let body = json!({
            "owner_id": owner_id,
            "source_kind": source_kind,
            "limit": limit,
        });
        self.post_json("tree/list_sources", &body).await
    }

    // ── analytics (in-process - runtime state, not externalized) ──────────

    fn analytics_user_stats(&self, agent_id: &str, active_days: u32) -> CarrierResult<Value> {
        self.fallback.analytics_user_stats(agent_id, active_days)
    }

    fn analytics_user_lookup(&self, agent_id: &str, sender_id: &str) -> CarrierResult<Value> {
        self.fallback.analytics_user_lookup(agent_id, sender_id)
    }

    fn analytics_usage(&self, agent_id: &str, days: u32) -> CarrierResult<Value> {
        self.fallback.analytics_usage(agent_id, days)
    }

    fn analytics_recent_conversations(&self, agent_id: &str, limit: u32) -> CarrierResult<Value> {
        self.fallback.analytics_recent_conversations(agent_id, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memory::MemorySubstrate;
    use types::memory_tree::GlobalQuery;

    /// Env-gated integration test: when `AGINXMEMORY_TEST_URL` is set (pointing
    /// at a running aginx-memory daemon), exercise the full HTTP round-trip the
    /// kernel now relies on at the 5 routed call sites - kv (sync trait ->
    /// block_in_place bridge) and tree_query_global (async). Skips otherwise.
    ///
    /// Run: start the daemon, then
    ///   AGINXMEMORY_TEST_URL=http://127.0.0.1:4399 \
    ///   cargo test -p runtime --features ... http_memory::tests::switch_on_round_trip
    #[tokio::test(flavor = "multi_thread")]
    async fn switch_on_round_trip() {
        let Some(url) = std::env::var("AGINXMEMORY_TEST_URL").ok() else {
            eprintln!("skip (set AGINXMEMORY_TEST_URL=http://127.0.0.1:<port>)");
            return;
        };
        let fallback = Arc::new(MemorySubstrate::open_in_memory().unwrap());
        let handle = HttpMemoryHandle::new(url, fallback).unwrap();

        // kv round-trip (sync trait method -> block_in_place -> HTTP).
        handle
            .kv_set("a1", "o1", "u1", "k", json!("v_round_trip"))
            .unwrap();
        let got = handle.kv_get("a1", "o1", "u1", "k").unwrap();
        assert_eq!(got, Some(json!("v_round_trip")));

        // tree_query_global (async -> HTTP). Empty owner -> empty hits, but must
        // not panic/error through the block_on bridge the kernel uses.
        let owner = "switch_on_owner".to_string();
        let req = GlobalQuery {
            owner_id: &owner,
            time_window_days: Some(7),
            query: None,
            limit: 3,
            user_id: None,
        };
        let resp = handle.tree_query_global(req).await.unwrap();
        assert_eq!(resp.total, 0);
        assert!(resp.hits.is_empty());
    }
}
