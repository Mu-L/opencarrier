//! MCP server connection management — connect, health-check, auto-reconnect.
//!
//! Handles connecting to configured MCP servers at boot, periodic health
//! monitoring, and exponential-backoff reconnection for failed servers.

use std::sync::Arc;
use tracing::{info, warn};

use crate::kernel::CarrierKernel;

impl CarrierKernel {
    /// Connect to all configured MCP servers and cache their tool definitions.
    pub(crate) async fn connect_mcp_servers(self: &Arc<Self>) {
        use runtime::mcp::{McpConnection, McpServerConfig, McpTransport};
        use types::config::McpTransportEntry;

        let servers = self
            .plugins
            .effective_mcp_servers
            .read()
            .map(|s| s.clone())
            .unwrap_or_default();

        for server_config in &servers {
            let transport = match &server_config.transport {
                McpTransportEntry::Stdio { command, args } => McpTransport::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                },
                McpTransportEntry::Sse { url } => McpTransport::Sse { url: url.clone() },
            };

            let mcp_config = McpServerConfig {
                name: server_config.name.clone(),
                transport,
                timeout_secs: server_config.timeout_secs,
                env: server_config.env.clone(),
            };

            match McpConnection::connect(mcp_config).await {
                Ok(conn) => {
                    let tool_count = conn.tools().len();
                    if let Ok(mut tools) = self.plugins.mcp_tools.lock() {
                        tools.extend(conn.tools().iter().cloned());
                    }
                    info!(
                        server = %server_config.name,
                        tools = tool_count,
                        "MCP server connected"
                    );
                    let key = runtime::mcp::normalize_name(&server_config.name);
                    self.plugins.mcp_connections.insert(key, conn);
                }
                Err(e) => {
                    warn!(
                        server = %server_config.name,
                        error = %e,
                        "Failed to connect to MCP server"
                    );
                }
            }
        }

        let tool_count = self.plugins.mcp_tools.lock().map(|t| t.len()).unwrap_or(0);
        if tool_count > 0 {
            info!(
                "MCP: {tool_count} tools available from {} server(s)",
                self.plugins.mcp_connections.len()
            );
        }

        self.spawn_mcp_health_monitor();
    }

    /// Background task that periodically checks MCP server health and
    /// reconnects any server that has gone down.
    fn spawn_mcp_health_monitor(self: &Arc<Self>) {
        let kernel = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;

                // --- Phase 1: Detect dead connections by pinging ---
                let mut dead_servers = Vec::new();
                {
                    let keys: Vec<String> = kernel
                        .plugins
                        .mcp_connections
                        .iter()
                        .map(|e| e.key().clone())
                        .collect();
                    for key in &keys {
                        if let Some(mut conn) = kernel.plugins.mcp_connections.get_mut(key) {
                            if conn.ping().await.is_err() {
                                let name = conn.name().to_string();
                                let config = conn.config().clone();
                                warn!(server = %name, "MCP server health check failed, will reconnect");
                                dead_servers.push((name, config));
                            }
                        }
                    }
                    let dead_keys: Vec<String> = dead_servers
                        .iter()
                        .map(|(name, _)| runtime::mcp::normalize_name(name))
                        .collect();
                    for key in &dead_keys {
                        kernel.plugins.mcp_connections.remove(key);
                    }
                }

                // --- Phase 2: Sync connections with effective config ---
                {
                    use runtime::mcp::normalize_name;

                    let effective: Vec<types::config::McpServerConfigEntry> = kernel
                        .plugins
                        .effective_mcp_servers
                        .read()
                        .map(|s| s.clone())
                        .unwrap_or_default();
                    let effective_keys: Vec<String> =
                        effective.iter().map(|c| normalize_name(&c.name)).collect();

                    {
                        let mut tools = kernel
                            .plugins
                            .mcp_tools
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let mut removed = Vec::new();
                        kernel.plugins.mcp_connections.retain(|key, conn| {
                            if effective_keys.contains(key) {
                                true
                            } else {
                                let prefix = format!("mcp_{key}_");
                                tools.retain(|t| !t.name.starts_with(&prefix));
                                removed.push(conn.name().to_string());
                                false
                            }
                        });
                        for name in &removed {
                            info!(server = %name, "MCP server removed (no longer in config)");
                        }
                    }

                    let connected_keys: Vec<String> = kernel
                        .plugins
                        .mcp_connections
                        .iter()
                        .map(|e| e.key().clone())
                        .collect();
                    let missing: Vec<&types::config::McpServerConfigEntry> = effective
                        .iter()
                        .filter(|c| !connected_keys.contains(&normalize_name(&c.name)))
                        .collect();

                    for server_config in &missing {
                        use runtime::mcp::{McpConnection, McpServerConfig, McpTransport};
                        use types::config::McpTransportEntry;

                        let transport = match &server_config.transport {
                            McpTransportEntry::Stdio { command, args } => McpTransport::Stdio {
                                command: command.clone(),
                                args: args.clone(),
                            },
                            McpTransportEntry::Sse { url } => {
                                McpTransport::Sse { url: url.clone() }
                            }
                        };
                        let mcp_config = McpServerConfig {
                            name: server_config.name.clone(),
                            transport,
                            timeout_secs: server_config.timeout_secs,
                            env: server_config.env.clone(),
                        };
                        info!(server = %server_config.name, "Connecting MCP server (found in config but not connected)");
                        match McpConnection::connect(mcp_config).await {
                            Ok(conn) => {
                                if let Ok(mut tools) = kernel.plugins.mcp_tools.lock() {
                                    tools.extend(conn.tools().iter().cloned());
                                }
                                let key = normalize_name(&server_config.name);
                                kernel.plugins.mcp_connections.insert(key, conn);
                                info!(server = %server_config.name, "MCP server connected");
                            }
                            Err(e) => {
                                warn!(server = %server_config.name, error = %e, "Failed to connect MCP server, will retry next cycle");
                            }
                        }
                    }
                }

                // --- Phase 3: Reconnect dead servers with exponential backoff ---
                for (name, config) in dead_servers {
                    use runtime::mcp::McpConnection;
                    let still_configured = kernel
                        .plugins
                        .effective_mcp_servers
                        .read()
                        .map(|s| s.iter().any(|c| c.name == name))
                        .unwrap_or(false);
                    if !still_configured {
                        kernel
                            .plugins
                            .mcp_reconnect_failures
                            .remove(&runtime::mcp::normalize_name(&name));
                        continue;
                    }
                    let key = runtime::mcp::normalize_name(&name);
                    let fail_count = kernel
                        .plugins
                        .mcp_reconnect_failures
                        .get(&key)
                        .map(|g| *g)
                        .unwrap_or(0);
                    if fail_count > 0 {
                        let backoff_secs = std::cmp::min(60 * 2u64.pow(fail_count), 3600);
                        if backoff_secs > 60 {
                            if fail_count >= 5 && fail_count % 6 == 5 {
                                warn!(
                                    server = %name,
                                    failures = fail_count,
                                    backoff_secs,
                                    "MCP server still unreachable after multiple attempts"
                                );
                            }
                            kernel
                                .plugins
                                .mcp_reconnect_failures
                                .insert(key.clone(), fail_count + 1);
                            continue;
                        }
                    }
                    info!(server = %name, "Attempting MCP server reconnection");
                    match McpConnection::connect(config).await {
                        Ok(conn) => {
                            let tool_count = conn.tools().len();
                            if let Ok(mut tools) = kernel.plugins.mcp_tools.lock() {
                                let prefix = format!("mcp_{key}");
                                tools.retain(|t| !t.name.starts_with(&prefix));
                                tools.extend(conn.tools().iter().cloned());
                            }
                            kernel.plugins.mcp_reconnect_failures.remove(&key);
                            kernel.plugins.mcp_connections.insert(key, conn);
                            info!(server = %name, tools = tool_count, "MCP server reconnected");
                        }
                        Err(e) => {
                            let new_count = fail_count + 1;
                            kernel.plugins.mcp_reconnect_failures.insert(key, new_count);
                            let backoff_secs = std::cmp::min(60 * 2u64.pow(new_count), 3600);
                            warn!(
                                server = %name,
                                error = %e,
                                consecutive_failures = new_count,
                                next_retry_in_secs = backoff_secs,
                                "MCP reconnection failed"
                            );
                        }
                    }
                }
            }
        });
    }
}
