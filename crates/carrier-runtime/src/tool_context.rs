//! Tool execution context — bundles all environment references needed by `execute_tool`.

use crate::browser::BrowserManager;
use crate::kernel_handle::KernelHandle;
use crate::llm_driver::Brain;
use crate::mcp::McpConnection;
use crate::process_manager::ProcessManager;
use crate::web_search::WebToolsContext;
use carrier_types::config::{DockerSandboxConfig, ExecPolicy};
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;

/// Environment context passed to every tool execution.
///
/// Groups the 15 optional references that `execute_tool` needs beyond the
/// per-call `(tool_use_id, tool_name, input)` triple.
///
/// All fields are `Option<&T>` — inherently `Copy` — so the struct derives `Copy`
/// and can be unpacked in one line at the top of `execute_tool`.
#[derive(Copy, Clone)]
pub struct ToolContext<'a> {
    pub kernel: Option<&'a Arc<dyn KernelHandle>>,
    pub allowed_tools: Option<&'a [String]>,
    pub caller_agent_id: Option<&'a str>,
    pub mcp_connections: Option<&'a DashMap<String, McpConnection>>,
    pub web_ctx: Option<&'a WebToolsContext>,
    pub browser_ctx: Option<&'a BrowserManager>,
    pub allowed_env_vars: Option<&'a [String]>,
    pub workspace_root: Option<&'a Path>,
    pub brain: Option<&'a Arc<dyn Brain>>,
    pub exec_policy: Option<&'a ExecPolicy>,
    pub docker_config: Option<&'a DockerSandboxConfig>,
    pub process_manager: Option<&'a ProcessManager>,
    pub sender_id: Option<&'a str>,
}
