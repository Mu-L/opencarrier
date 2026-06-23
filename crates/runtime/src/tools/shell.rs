//! Shell execution tool module.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::config::ExecSecurityMode;
use types::taint::{TaintLabel, TaintSink, TaintedValue};
use types::tool::ToolDefinition;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use tracing::warn;

/// Shell execution tools.
pub struct ShellTools;

#[async_trait]
impl ToolModule for ShellTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "shell_exec".to_string(),
            description: "Execute a shell command and return its output.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to execute" },
                    "timeout_seconds": { "type": "integer", "description": "Timeout in seconds (default: 30)" }
                },
                "required": ["command"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "shell_exec" {
            return None;
        }

        let command = input["command"].as_str().unwrap_or("");
        let exec_policy = ctx.exec_policy;
        let allowed_env = ctx.allowed_env_vars.unwrap_or(&[]);
        let workspace_root = ctx.workspace_root;

        // SECURITY: Always check for shell metacharacters, even in Full mode.
        if let Some(reason) = crate::subprocess_sandbox::contains_shell_metacharacters(command) {
            return Some(Err(format!(
                "shell_exec blocked: command contains {reason}. \
                 Shell metacharacters are never allowed."
            )));
        }

        // Exec policy enforcement (allowlist / deny / full)
        if let Some(policy) = exec_policy {
            if let Err(reason) =
                crate::subprocess_sandbox::validate_command_allowlist(command, policy)
            {
                return Some(Err(format!(
                    "shell_exec blocked: {reason}. Current exec_policy.mode = '{:?}'. \
                     To allow shell commands, set exec_policy.mode = 'full' in the agent manifest or config.toml.",
                    policy.mode
                )));
            }
        }

        // Skip heuristic taint patterns for Full exec policy
        let is_full_exec = exec_policy.is_some_and(|p| p.mode == ExecSecurityMode::Full);
        if !is_full_exec {
            let suspicious_patterns = ["curl ", "wget ", "| sh", "| bash", "base64 -d", "eval "];
            for pattern in &suspicious_patterns {
                if command.contains(pattern) {
                    let mut labels = HashSet::new();
                    labels.insert(TaintLabel::ExternalNetwork);
                    let tainted = TaintedValue::new(command, labels, "llm_tool_call");
                    if let Err(violation) = tainted.check_sink(&TaintSink::shell_exec()) {
                        warn!(
                            command = crate::str_utils::safe_truncate_str(command, 80),
                            %violation,
                            "Shell taint check failed"
                        );
                        return Some(Err(format!("Taint violation: {violation}")));
                    }
                }
            }
        }

        Some(exec_shell(input, allowed_env, workspace_root, exec_policy).await)
    }

    fn permission_level(&self, _tool_name: &str) -> types::tool::PermissionLevel {
        // shell_exec is the most dangerous tool — irreversible system access
        types::tool::PermissionLevel::Dangerous
    }
}

async fn exec_shell(
    input: &Value,
    allowed_env: &[String],
    workspace_root: Option<&Path>,
    exec_policy: Option<&types::config::ExecPolicy>,
) -> Result<String, String> {
    let command = input["command"]
        .as_str()
        .ok_or("Missing 'command' parameter")?;
    let policy_timeout = exec_policy.map(|p| p.timeout_secs).unwrap_or(30);
    let timeout_secs = input["timeout_seconds"].as_u64().unwrap_or(policy_timeout);

    let use_direct_exec = exec_policy
        .map(|p| p.mode == ExecSecurityMode::Allowlist)
        .unwrap_or(true);

    let mut cmd = if use_direct_exec {
        let argv = shlex::split(command).ok_or_else(|| {
            "Command contains unmatched quotes or invalid shell syntax".to_string()
        })?;
        if argv.is_empty() {
            return Err("Empty command after parsing".to_string());
        }
        let mut c = tokio::process::Command::new(&argv[0]);
        if argv.len() > 1 {
            c.args(&argv[1..]);
        }
        c
    } else {
        #[cfg(windows)]
        let git_sh: Option<&str> = {
            const SH_PATHS: &[&str] = &[
                "C:\\Program Files\\Git\\usr\\bin\\sh.exe",
                "C:\\Program Files (x86)\\Git\\usr\\bin\\sh.exe",
            ];
            SH_PATHS
                .iter()
                .copied()
                .find(|p| std::path::Path::new(p).exists())
        };
        let (shell, shell_arg) = if cfg!(windows) {
            #[cfg(windows)]
            {
                if let Some(sh) = git_sh {
                    (sh, "-c")
                } else {
                    ("cmd", "/C")
                }
            }
            #[cfg(not(windows))]
            {
                ("sh", "-c")
            }
        } else {
            ("sh", "-c")
        };
        let mut c = tokio::process::Command::new(shell);
        c.arg(shell_arg).arg(command);
        c
    };

    if let Some(ws) = workspace_root {
        cmd.current_dir(ws);
    }

    crate::subprocess_sandbox::sandbox_command(&mut cmd, allowed_env);

    #[cfg(windows)]
    cmd.env("PYTHONIOENCODING", "utf-8");

    cmd.stdin(std::process::Stdio::null());

    let result =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output()).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let max_output = 100_000;
            let stdout_str = if stdout.len() > max_output {
                format!(
                    "{}...\n[truncated, {} total bytes]",
                    crate::str_utils::safe_truncate_str(&stdout, max_output),
                    stdout.len()
                )
            } else {
                stdout.to_string()
            };
            let stderr_str = if stderr.len() > max_output {
                format!(
                    "{}...\n[truncated, {} total bytes]",
                    crate::str_utils::safe_truncate_str(&stderr, max_output),
                    stderr.len()
                )
            } else {
                stderr.to_string()
            };

            Ok(format!(
                "Exit code: {exit_code}\n\nSTDOUT:\n{stdout_str}\nSTDERR:\n{stderr_str}"
            ))
        }
        Ok(Err(e)) => Err(format!("Failed to execute command: {e}")),
        Err(_) => Err(format!("Command timed out after {timeout_secs}s")),
    }
}

// ---------------------------------------------------------------------------
// cli_exec — whitelisted CLI command execution
// ---------------------------------------------------------------------------

/// Whitelisted CLI command execution tool.
///
/// Unlike `shell_exec` (Dangerous), `cli_exec` only allows commands
/// explicitly listed in the config. Arguments are parsed with `shlex`
/// and executed directly — no shell wrapper. Safe for low-privilege agents.
pub struct CliExecTools {
    config: types::config::CliExecConfig,
}

impl CliExecTools {
    pub fn new(config: types::config::CliExecConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ToolModule for CliExecTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if self.config.commands.is_empty() {
            return vec![];
        }

        // Build a description that lists available commands and examples.
        let mut cmd_lines = Vec::new();
        for cmd in &self.config.commands {
            let examples = if cmd.examples.is_empty() {
                String::new()
            } else {
                format!(" (e.g. {})", cmd.examples.join(", "))
            };
            cmd_lines.push(format!("- {}: {}{}", cmd.name, cmd.description, examples));
        }
        let description = format!(
            "Execute a whitelisted CLI command. Available commands:\n{}",
            cmd_lines.join("\n")
        );

        vec![ToolDefinition {
            name: "cli_exec".to_string(),
            description,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command name (e.g. 'gh', 'todoist')" },
                    "args": { "type": "string", "description": "Arguments as a single string (e.g. 'pr list --repo owner/repo')" }
                },
                "required": ["command"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "cli_exec" {
            return None;
        }

        let command_name = input["command"].as_str().unwrap_or("").trim();

        // 1. Check whitelist
        let allowed = self.config.commands.iter().find(|c| c.name == command_name);
        if allowed.is_none() {
            let available: Vec<&str> = self.config.commands.iter().map(|c| c.name.as_str()).collect();
            return Some(Err(format!(
                "Command '{command_name}' not in cli_exec allowlist. Available: {}",
                available.join(", ")
            )));
        }

        // 2. Parse args with shlex — never start a shell
        let args_str = input["args"].as_str().unwrap_or("");
        let mut argv = vec![command_name.to_string()];
        if !args_str.is_empty() {
            // SECURITY: reject shell metacharacters in args
            if let Some(reason) = crate::subprocess_sandbox::contains_shell_metacharacters(args_str) {
                return Some(Err(format!(
                    "cli_exec blocked: args contain {reason}. \
                     Shell metacharacters (pipes, redirects, subshells) are not allowed."
                )));
            }
            let parsed = shlex::split(args_str).ok_or_else(|| {
                "Arguments contain unmatched quotes or invalid syntax".to_string()
            });
            match parsed {
                Ok(parts) => argv.extend(parts),
                Err(e) => return Some(Err(e)),
            }
        }

        // 3. Execute directly — no shell wrapper
        let allowed_env = ctx.allowed_env_vars.unwrap_or(&[]);
        let workspace_root = ctx.workspace_root;

        let mut cmd = tokio::process::Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }

        if let Some(ws) = workspace_root {
            cmd.current_dir(ws);
        }

        crate::subprocess_sandbox::sandbox_command(&mut cmd, allowed_env);

        #[cfg(windows)]
        cmd.env("PYTHONIOENCODING", "utf-8");

        cmd.stdin(std::process::Stdio::null());

        let timeout_secs = 30u64;
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);

                let max_output = 100_000;
                let stdout_str = if stdout.len() > max_output {
                    format!(
                        "{}...\n[truncated, {} total bytes]",
                        crate::str_utils::safe_truncate_str(&stdout, max_output),
                        stdout.len()
                    )
                } else {
                    stdout.to_string()
                };
                let stderr_str = if stderr.len() > max_output {
                    format!(
                        "{}...\n[truncated, {} total bytes]",
                        crate::str_utils::safe_truncate_str(&stderr, max_output),
                        stderr.len()
                    )
                } else {
                    stderr.to_string()
                };

                Some(Ok(format!(
                    "Exit code: {exit_code}\n\nSTDOUT:\n{stdout_str}\nSTDERR:\n{stderr_str}"
                )))
            }
            Ok(Err(e)) => Some(Err(format!("Failed to execute command: {e}"))),
            Err(_) => Some(Err(format!("Command timed out after {timeout_secs}s"))),
        }
    }

    fn permission_level(&self, _tool_name: &str) -> types::tool::PermissionLevel {
        // cli_exec is restricted to whitelisted commands only — safe for Write-level agents
        types::tool::PermissionLevel::Write
    }
}
