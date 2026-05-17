//! Trait abstraction for kernel operations needed by the agent runtime.
//!
//! This trait allows `carrier-runtime` to call back into the kernel for
//! inter-agent operations (spawn, send, list, kill) without creating
//! a circular dependency. The kernel implements this trait and passes
//! it into the agent loop.

use async_trait::async_trait;

/// Agent info returned by list and discovery operations.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub state: String,
    pub modality: String,
    pub model: String,
    pub description: String,
    pub tags: Vec<String>,
    pub tools: Vec<String>,
}

/// Handle to kernel operations, passed into the agent loop so agents
/// can interact with each other via tools.
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait KernelHandle: Send + Sync {
    /// Spawn a new agent from a TOML manifest string.
    /// `parent_id` is the UUID string of the spawning agent (for lineage tracking).
    /// Returns (agent_id, agent_name) on success.
    async fn spawn_agent(
        &self,
        manifest_toml: &str,
        parent_id: Option<&str>,
    ) -> Result<(String, String), String>;

    /// Send a message to another agent and get the response.
    /// `sender_id` and `sender_name` identify the originating user (e.g. WeChat user).
    /// `caller_agent_id` is the agent invoking this tool, used for tenant isolation.
    /// `owner_id` is the route owner (the person who created the bot). When None,
    /// defaults to sender_id for backward compatibility.
    async fn send_to_agent(
        &self,
        agent_id: &str,
        message: &str,
        sender_id: Option<&str>,
        sender_name: Option<&str>,
        caller_agent_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
    ) -> Result<String, String>;

    /// List all running agents visible to the caller.
    fn list_agents(&self) -> Vec<AgentInfo>;

    /// Kill an agent by ID.
    fn kill_agent(&self, agent_id: &str) -> Result<(), String>;

    /// Restart an agent by ID (reset state, re-read manifest from workspace).
    fn restart_agent(&self, agent_id: &str) -> Result<(), String>;

    /// Store a key-value pair in the system KV store (for schedules, config, etc.).
    fn system_kv_store(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), String>;

    /// Recall a value from the system KV store.
    fn system_kv_recall(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, String>;

    /// Find agents by query (matches on name substring, tag, or tool name; case-insensitive).
    fn find_agents(&self, query: &str) -> Vec<AgentInfo>;

    /// Post a task to the shared task queue. Returns the task ID.
    async fn task_post(
        &self,
        title: &str,
        description: &str,
        assigned_to: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<String, String>;

    /// Claim the next available task.
    async fn task_claim(&self, agent_id: &str) -> Result<Option<serde_json::Value>, String>;

    /// Mark a task as completed with a result string.
    async fn task_complete(&self, task_id: &str, result: &str) -> Result<(), String>;

    /// List tasks, optionally filtered by status.
    async fn task_list(&self, status: Option<&str>) -> Result<Vec<serde_json::Value>, String>;

    /// Publish a custom event that can trigger proactive agents.
    async fn publish_event(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), String>;

    /// Create a cron job for the calling agent.
    async fn cron_create(
        &self,
        agent_id: &str,
        owner_id: Option<&str>,
        job_json: serde_json::Value,
    ) -> Result<String, String> {
        let _ = (agent_id, owner_id, job_json);
        Err("Cron scheduler not available".to_string())
    }

    /// List cron jobs for the calling agent, optionally filtered by owner_id.
    async fn cron_list(&self, agent_id: &str, owner_id: Option<&str>) -> Result<Vec<serde_json::Value>, String> {
        let _ = (agent_id, owner_id);
        Err("Cron scheduler not available".to_string())
    }

    /// Cancel a cron job by ID.
    async fn cron_cancel(&self, job_id: &str) -> Result<(), String> {
        let _ = job_id;
        Err("Cron scheduler not available".to_string())
    }

    /// List discovered external A2A agents as (name, url) pairs.
    fn list_a2a_agents(&self) -> Vec<(String, String)> {
        vec![]
    }

    /// Get the URL of a discovered external A2A agent by name.
    fn get_a2a_agent_url(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }

    /// Resolve an agent's workspace directory by name.
    /// Returns the absolute path string, or None if the agent is not found.
    fn resolve_agent_workspace(&self, agent_name: &str) -> Option<String> {
        let _ = agent_name;
        None
    }

    /// Rebuild the available tool list for an agent.
    /// Used after mid-loop skill installations (e.g., train_write) so the
    /// LLM can use newly installed tools in the next iteration.
    fn refresh_tools(
        &self,
        agent_id_str: &str,
    ) -> Option<Vec<types::tool::ToolDefinition>> {
        let _ = agent_id_str;
        None
    }

    /// Activate a toolset for an agent's session and return refreshed tool list.
    fn activate_toolset(
        &self,
        agent_id_str: &str,
        toolset_name: &str,
    ) -> Option<Vec<types::tool::ToolDefinition>> {
        let _ = (agent_id_str, toolset_name);
        None
    }

    /// Search the tool catalog for tools matching a query.
    /// Returns (toolset_name, ToolDefinition) pairs ranked by relevance.
    fn search_tools(
        &self,
        query: &str,
        limit: usize,
    ) -> Vec<(String, types::tool::ToolDefinition)> {
        let _ = (query, limit);
        Vec::new()
    }

    /// Get the home directory path (~/.opencarrier/).
    fn home_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    // -----------------------------------------------------------------
    // Tree memory operations
    // -----------------------------------------------------------------

    /// Ingest messages into the tree memory system.
    async fn tree_ingest(
        &self,
        req: types::memory_tree::IngestRequest,
    ) -> Result<types::memory_tree::IngestResult, String> {
        let _ = req;
        Err("Tree memory not available".to_string())
    }

    /// Query source tree summaries.
    async fn tree_query_source(
        &self,
        req: types::memory_tree::SourceQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        let _ = req;
        Err("Tree memory not available".to_string())
    }

    /// Query global tree summaries.
    async fn tree_query_global(
        &self,
        req: types::memory_tree::GlobalQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        let _ = req;
        Err("Tree memory not available".to_string())
    }

    /// Query topic tree by entity.
    async fn tree_query_topic(
        &self,
        req: types::memory_tree::TopicQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        let _ = req;
        Err("Tree memory not available".to_string())
    }

    /// Search entities by substring.
    async fn tree_search_entities(
        &self,
        req: types::memory_tree::EntitySearch<'_>,
    ) -> Result<Vec<types::memory_tree::EntityMatch>, String> {
        let _ = req;
        Err("Tree memory not available".to_string())
    }

    /// Drill down from a summary to its children.
    async fn tree_drill_down(
        &self,
        req: types::memory_tree::DrillDownQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        let _ = req;
        Err("Tree memory not available".to_string())
    }

    /// Fetch all leaf chunks under a summary.
    async fn tree_fetch_leaves(
        &self,
        req: types::memory_tree::FetchLeavesQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        let _ = req;
        Err("Tree memory not available".to_string())
    }

    /// List all source trees for an owner.
    async fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<types::memory_tree::TreeSummary>, String> {
        let _ = (owner_id, source_kind, limit);
        Err("Tree memory not available".to_string())
    }

    /// Spawn an agent with capability inheritance enforcement.
    /// `parent_caps` are the parent's granted capabilities. The kernel MUST verify
    /// that every capability in the child manifest is covered by `parent_caps`.
    async fn spawn_agent_checked(
        &self,
        manifest_toml: &str,
        parent_id: Option<&str>,
        parent_caps: &[types::capability::Capability],
    ) -> Result<(String, String), String> {
        // Default: delegate to spawn_agent (no enforcement)
        // The kernel MUST override this with real enforcement
        let _ = parent_caps;
        self.spawn_agent(manifest_toml, parent_id).await
    }

}
