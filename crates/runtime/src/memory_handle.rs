//! Memory handle trait — the carrier's independent memory service.
//!
//! Like Brain (LLM calls) and KernelHandle (inter-agent operations),
//! MemoryHandle is a top-level service handle injected into the agent
//! loop and ToolContext. It provides two capabilities:
//!
//! - **kv**: structured key-value storage (credentials, preferences, summaries)
//! - **tree**: hierarchical conversation history retrieval
//!
//! Both are scoped by (agent_id, owner_id, user_id) for multi-user isolation.

use async_trait::async_trait;

/// Handle to memory operations, passed into the agent loop and tools.
///
/// Implemented by CarrierKernel by delegating to MemorySubstrate.
#[async_trait]
pub trait MemoryHandle: Send + Sync {
    // -----------------------------------------------------------------
    // KV operations — structured key-value storage
    // -----------------------------------------------------------------

    /// Store a key-value pair in the user's private memory.
    fn kv_set(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), String>;

    /// Retrieve a value from the user's private memory by key.
    fn kv_get(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, String>;

    /// List all key-value pairs for a given agent + user.
    fn kv_list(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, String>;

    /// Delete a key-value pair from the user's private memory.
    fn kv_delete(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<(), String>;

    // -----------------------------------------------------------------
    // Tree memory operations — conversation history retrieval
    // -----------------------------------------------------------------

    /// Ingest messages into the tree memory system.
    async fn tree_ingest(
        &self,
        req: types::memory_tree::IngestRequest,
    ) -> Result<types::memory_tree::IngestResult, String>;

    /// Query source-scoped tree summaries.
    async fn tree_query_source(
        &self,
        req: types::memory_tree::SourceQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String>;

    /// Query global tree summaries.
    async fn tree_query_global(
        &self,
        req: types::memory_tree::GlobalQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String>;

    /// Query topic-scoped tree by entity.
    async fn tree_query_topic(
        &self,
        req: types::memory_tree::TopicQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String>;

    /// Search entities by substring.
    async fn tree_search_entities(
        &self,
        req: types::memory_tree::EntitySearch<'_>,
    ) -> Result<Vec<types::memory_tree::EntityMatch>, String>;

    /// Drill down from a summary node to its children.
    async fn tree_drill_down(
        &self,
        req: types::memory_tree::DrillDownQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String>;

    /// Fetch all leaf chunks under a summary node.
    async fn tree_fetch_leaves(
        &self,
        req: types::memory_tree::FetchLeavesQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String>;

    /// List all source trees for an owner.
    async fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<types::memory_tree::TreeSummary>, String>;

    // -----------------------------------------------------------------
    // Analytics operations (for data_analyze tool)
    // -----------------------------------------------------------------

    /// User statistics: total users, active users, new users.
    fn analytics_user_stats(&self, agent_id: &str, active_days: u32) -> Result<serde_json::Value, String>;

    /// Per-user lookup: session count, last active, recent conversation summary.
    fn analytics_user_lookup(&self, agent_id: &str, sender_id: &str) -> Result<serde_json::Value, String>;

    /// Usage analytics: token consumption, daily trends, per-model breakdown.
    fn analytics_usage(&self, agent_id: &str, days: u32) -> Result<serde_json::Value, String>;

    /// Recent conversations list (metadata only, no message content).
    fn analytics_recent_conversations(&self, agent_id: &str, limit: u32) -> Result<serde_json::Value, String>;
}
