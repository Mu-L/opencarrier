//! Single-step runners: agent_loop, chat, tool.


use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::warn;

use runtime::agent_loop::{
    run_agent_loop, TOOL_LONG_TIMEOUT_NAMES, TOOL_TIMEOUT_LONG_SECS, TOOL_TIMEOUT_SECS,
};
use runtime::kernel_handle::KernelHandle;
use runtime::llm_driver::{Brain, CompletionRequest};
use runtime::plugin::admin_store::is_admin;
use runtime::tool_context::ToolContext;
use runtime::tool_runner::execute_tool;
use types::agent::{AgentId, AgentManifest};
use types::error::CarrierError;
use types::flow::StepDef;
use types::message::{Message, TokenUsage};
use types::tool::ToolResult;

use crate::error::{KernelError, KernelResult};
use crate::kernel::CarrierKernel;
use super::template::{render_template, render_value, select_output};

impl CarrierKernel {

    /// Run a single `agent_loop` step in its own fresh session and return its
    /// output value + usage. Shared by `run_flow` (top-level) and
    /// `exec_body_steps` (map body). Resolves the driver + memory handle here
    /// so callers need not thread them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_step_agent_loop(
        &self,
        step: &StepDef,
        step_prompt: &str,
        step_user_msg: &str,
        base_system_prompt: &str,
        agent_name: &str,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        let driver = self.resolve_driver(manifest)?;
        let memory_handle: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
            Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(
                &self.memory,
            ))));
        // base_system_prompt already carries the flow body (injected by
        // prepare_agent_context); add only the step directive.
        let step_system = format!(
            "{base_system_prompt}\n\n## 当前步骤: {}\n{step_prompt}",
            step.id,
        );
        let mut step_manifest = manifest.clone();
        step_manifest.model.system_prompt = step_system;
        let mut step_session = self
            .memory
            .create_session_async(agent_name.to_string())
            .await
            .map_err(KernelError::Carrier)?;
        let r = run_agent_loop(
            &step_manifest,
            step_user_msg,
            &mut step_session,
            &self.memory,
            driver,
            tools,
            kernel_handle,
            None,
            Some(&self.plugins.mcp_connections),
            Some(&self.services.fetch_engine),
            manifest.workspace.as_deref(),
            None,
            Some(&self.coordination.hooks),
            None,
            Some(&self.coordination.process_manager),
            None,
            brain.cloned(),
            memory_handle,
            sender_id,
            owner_id,
            channel_type,
            Some(self.runtime.llm_concurrency_limit.clone()),
        )
        .await
        .map_err(KernelError::Carrier)?;
        let out_val = select_output(step, &r.response, outputs, input)?;
        Ok((out_val, r.total_usage, r.iterations))
    }

    /// Run a single `chat` step (one-shot LLM completion, no tools) and return
    /// its output value + usage. Shared by `run_flow` and `exec_body_steps`.
    pub(crate) async fn run_step_chat(
        &self,
        step: &StepDef,
        step_user_msg: &str,
        base_system_prompt: &str,
        brain: Option<&Arc<dyn Brain>>,
        outputs: &HashMap<String, Value>,
        input: &Value,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        let brain_ref = brain.ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal("chat step requires a brain".into()))
        })?;
        let task_text = step
            .task
            .as_deref()
            .map(|t| render_template(t, outputs, input))
            .unwrap_or_else(|| step_user_msg.to_string());
        let system = format!("{base_system_prompt}\n\n## 当前步骤: {}\n{task_text}", step.id);
        let req = CompletionRequest {
            model: String::new(),
            messages: vec![Message::user(step_user_msg.to_string())],
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: 0.7,
            system: Some(system),
            thinking: None,
            extra: Default::default(),
        };
        let resp = brain_ref
            .complete("fast", req)
            .await
            .map_err(KernelError::Carrier)?;
        let final_msg = resp.text();
        let out_val = select_output(step, &final_msg, outputs, input)?;
        Ok((out_val, resp.usage, 1))
    }

    /// Run a single `tool` step: resolve the tool by name, render `tool_args`
    /// templates, execute it via the shared `execute_tool` (with permission +
    /// admin-gate + timeout), and return its output. A tool error becomes an
    /// `Err` so `run_flow`'s `on_failure` can degrade. Shared by `run_flow`
    /// (top-level) and `exec_body_steps` (map body).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_step_tool(
        &self,
        step: &StepDef,
        agent_id: AgentId,
        manifest: &AgentManifest,
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        let tool_name = step.tool_name.as_deref().ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "tool step '{}' missing `tool`/`tool_name`",
                step.id
            )))
        })?;
        let rendered_args = render_value(&step.tool_args, outputs, input);

        // Assemble the ToolContext (mirrors runtime/agent_loop/tool_use.rs).
        let memory_handle: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
            Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(
                &self.memory,
            ))));
        let caller_id = agent_id.to_string();
        let workspace_root: Option<&Path> = manifest.workspace.as_deref();
        let is_clone_admin =
            matches!((sender_id, workspace_root), (Some(sid), Some(root)) if is_admin(root, sid));
        let tool_ctx = ToolContext {
            kernel: kernel_handle.as_ref(),
            memory: memory_handle.as_ref(),
            caller_agent_id: Some(&caller_id),
            mcp_connections: Some(&self.plugins.mcp_connections),
            fetch_engine: Some(&self.services.fetch_engine),
            allowed_env_vars: None,
            workspace_root,
            brain,
            exec_policy: manifest.exec_policy.as_ref(),
            cli_exec_config: manifest.cli_exec.as_ref(),
            process_manager: Some(&self.coordination.process_manager),
            sender_id,
            owner_id,
            home_dir: Some(self.config.home_dir.as_path()),
            agent_name: Some(agent_name),
            subagent_configs: if manifest.subagents.is_empty() {
                None
            } else {
                Some(&manifest.subagents)
            },
            channel_type,
            max_tool_level: manifest.max_tool_level,
            is_clone_admin,
        };

        let tool_use_id = format!("flow:{}:{}", step.id, tool_name);
        let timeout_secs = if TOOL_LONG_TIMEOUT_NAMES.contains(&tool_name) {
            TOOL_TIMEOUT_LONG_SECS
        } else {
            TOOL_TIMEOUT_SECS
        };
        let result = match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            execute_tool(&tool_use_id, tool_name, &rendered_args, &tool_ctx),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                warn!(
                    flow = "tool_step",
                    step = %step.id,
                    tool = tool_name,
                    "tool step timed out after {}s",
                    timeout_secs
                );
                ToolResult {
                    tool_use_id,
                    content: format!("Tool '{}' timed out after {}s.", tool_name, timeout_secs),
                    is_error: true,
                }
            }
        };

        if result.is_error {
            return Err(KernelError::Carrier(CarrierError::Internal(format!(
                "tool step '{}' ('{}') failed: {}",
                step.id, tool_name, result.content
            ))));
        }
        // Tool content is often JSON; parse to a structured value when possible,
        // else keep the raw string.
        let out_val = serde_json::from_str::<Value>(&result.content)
            .unwrap_or_else(|_| Value::String(result.content.clone()));
        // Tool execution uses no LLM tokens; count as one iteration.
        Ok((out_val, TokenUsage::default(), 1))
    }
}
