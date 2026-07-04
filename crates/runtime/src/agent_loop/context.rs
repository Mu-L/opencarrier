//! LoopContext — bundles all mutable state and references for the agent loop.
//!
//! The agent loop has many moving parts (session, messages, tools, MCP connections,
//! hooks, etc.) that are passed through the iteration cycle. Rather than threading
//! 20+ parameters through every function, we bundle them here so each phase function
//! takes `&mut LoopContext`.

use std::path::Path;
use std::sync::Arc;

use crate::context_budget::ContextBudget;
use crate::kernel_handle::KernelHandle;
use crate::llm_driver::{Brain, LlmDriver, StreamEvent};
use crate::mcp::McpConnection;
use crate::web_fetch::WebFetchEngine;
use memory::session::Session;
use memory::MemorySubstrate;
use types::agent::AgentManifest;
use types::message::Message;
use types::tool::ToolDefinition;

use super::state::LoopState;
use super::{PhaseCallback, TaskPlan};

/// Bundles all mutable state and shared references for a single agent loop execution.
///
/// Created once during setup, passed through each phase, and consumed during teardown.
pub(super) struct LoopContext<'a> {
    // ---- Agent identity ----
    pub manifest: &'a AgentManifest,
    pub user_message: &'a str,
    pub agent_id_str: String,

    // ---- Session & memory ----
    pub session: &'a mut Session,
    pub messages: Vec<Message>,
    pub session_base_len: usize,
    pub memory: &'a MemorySubstrate,
    pub memory_handle: Option<Arc<dyn crate::memory_handle::MemoryHandle>>,

    // ---- LLM ----
    pub driver: Arc<dyn LlmDriver>,
    pub brain: Option<Arc<dyn Brain>>,
    pub system_prompt: String,
    pub stream_tx: Option<tokio::sync::mpsc::Sender<StreamEvent>>,
    pub llm_concurrency_limit: Option<Arc<tokio::sync::Semaphore>>,

    // ---- Tools ----
    pub tools_owned: Vec<ToolDefinition>,
    pub discovered_tool_names: std::collections::HashSet<String>,
    pub loaded_skills: std::collections::HashSet<String>,

    // ---- Kernel & external ----
    pub kernel: Option<Arc<dyn KernelHandle>>,
    pub mcp_connections: Option<&'a dashmap::DashMap<String, McpConnection>>,
    pub fetch_engine: Option<&'a WebFetchEngine>,
    pub workspace_root: Option<&'a Path>,
    pub process_manager: Option<&'a crate::process_manager::ProcessManager>,
    pub context_budget: ContextBudget,

    // ---- Callbacks ----
    pub on_phase: Option<&'a PhaseCallback>,
    pub hooks: Option<&'a crate::hooks::HookRegistry>,

    // ---- Routing ----
    pub sender_id: Option<&'a str>,
    pub owner_id: Option<&'a str>,
    pub channel_type: Option<&'a str>,

    // ---- Config ----
    pub hand_allowed_env: Vec<String>,
    pub context_window_tokens: usize,

    // ---- Loop state ----
    pub state: LoopState,
    pub detected_plan: Option<TaskPlan>,
}

impl<'a> LoopContext<'a> {
    /// Get the current tools slice (borrows from tools_owned).
    /// Must be called fresh each time since tools_owned may have been modified.
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools_owned
    }

    /// Persist the last run summary to cross-session storage via kv_set.
    pub fn persist_last_run(&self, outcome: super::state::RunOutcome) {
        let last_run = self.state.to_last_run(outcome);
        if let Some(mh) = &self.memory_handle {
            let agent_key = format!("loop_state:{}", self.manifest.name);
            if let Ok(val) = serde_json::to_value(&last_run) {
                if let Err(e) = mh.kv_set(
                    &self.manifest.name,
                    self.owner_id.unwrap_or(""),
                    self.sender_id.unwrap_or(""),
                    &agent_key,
                    val,
                ) {
                    tracing::warn!("Failed to persist last run summary: {e}");
                }
            }
        }
    }
}
