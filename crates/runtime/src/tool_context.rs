//! Tool execution context — bundles all environment references needed by `execute_tool`.

use crate::kernel_handle::KernelHandle;
use crate::llm_driver::Brain;
use crate::memory_handle::MemoryHandle;
use crate::mcp::McpConnection;
use crate::process_manager::ProcessManager;
use crate::web_fetch::WebFetchEngine;
use types::agent::SubagentConfig;
use types::config::ExecPolicy;
use types::tool::PermissionLevel;
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;

/// Environment context passed to every tool execution.
///
/// Groups the optional references that `execute_tool` needs beyond the
/// per-call `(tool_use_id, tool_name, input)` triple.
///
/// All fields are `Option<&T>` — inherently `Copy` — so the struct derives `Copy`
/// and can be unpacked in one line at the top of `execute_tool`.
#[derive(Copy, Clone)]
pub struct ToolContext<'a> {
    pub kernel: Option<&'a Arc<dyn KernelHandle>>,
    pub memory: Option<&'a Arc<dyn MemoryHandle>>,
    pub caller_agent_id: Option<&'a str>,
    pub mcp_connections: Option<&'a DashMap<String, McpConnection>>,
    pub fetch_engine: Option<&'a WebFetchEngine>,
    pub allowed_env_vars: Option<&'a [String]>,
    pub workspace_root: Option<&'a Path>,
    pub brain: Option<&'a Arc<dyn Brain>>,
    pub exec_policy: Option<&'a ExecPolicy>,
    pub process_manager: Option<&'a ProcessManager>,
    pub sender_id: Option<&'a str>,
    pub owner_id: Option<&'a str>,
    pub home_dir: Option<&'a Path>,
    pub agent_name: Option<&'a str>,
    pub subagent_configs: Option<&'a [SubagentConfig]>,
    pub channel_type: Option<&'a str>,
    pub max_tool_level: PermissionLevel,
}
