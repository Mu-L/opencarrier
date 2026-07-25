//! Plugin dependency resolution and clone upgrade management.
//!
//! Handles downloading missing plugins from Hub and upgrading clone agents
//! with the latest template files while preserving user data.

use crate::kernel::CarrierKernel;
use runtime::kernel_handle::KernelHandle;
use tracing::info;

impl CarrierKernel {
    /// Resolve plugin dependencies for a newly installed clone.
    /// Downloads missing plugins from Hub.
    pub async fn resolve_plugin_dependencies(&self, plugins: &[String]) {
        let plugins_dir = match &self.config.plugins_dir {
            Some(dir) => dir.clone(),
            None => {
                let dir = self.config.home_dir.join("plugins");
                tracing::info!(
                    "No plugins_dir configured, using default: {}",
                    dir.display()
                );
                dir
            }
        };

        // Ensure plugins directory exists
        if let Err(e) = std::fs::create_dir_all(&plugins_dir) {
            tracing::warn!(
                "Failed to create plugins dir {}: {e}",
                plugins_dir.display()
            );
            return;
        }

        let hub_url = self.config.hub.url.trim_end_matches('/').to_string();
        let api_key_env = &self.config.hub.api_key_env;
        let api_key = match std::env::var(api_key_env) {
            Ok(k) => k,
            Err(_) => {
                tracing::warn!(
                    "Hub API key not set (env: {}), skipping plugin dependency resolution",
                    api_key_env
                );
                return;
            }
        };

        let mut installed = Vec::new();
        let mut failed = Vec::new();

        for plugin_name in plugins {
            if clone::hub::is_plugin_installed(&plugins_dir, plugin_name) {
                tracing::info!(plugin = %plugin_name, "Plugin already installed, skipping");
                continue;
            }

            tracing::info!(plugin = %plugin_name, "Downloading missing plugin from Hub...");
            match clone::hub::install_plugin(
                &hub_url,
                &api_key,
                plugin_name,
                None,
                &plugins_dir,
            )
            .await
            {
                Ok(_) => {
                    tracing::info!(plugin = %plugin_name, "Plugin installed successfully");
                    installed.push(plugin_name.clone());
                }
                Err(e) => {
                    tracing::warn!(plugin = %plugin_name, error = %e, "Failed to install plugin");
                    failed.push(plugin_name.clone());
                }
            }
        }

        if !installed.is_empty() || !failed.is_empty() {
            tracing::info!(
                installed = ?installed,
                failed = ?failed,
                "Plugin dependency resolution complete (restart required to load new plugins)"
            );
        }
    }

    /// Upgrade a clone from DupHub: fetch definition files (Bearer, file-level
    /// dup), apply definition layer only, preserve sessions/senders/output,
    /// rebuild agent.toml, restart.
    ///
    /// `version`: `None` = latest. Hub key = `hub_template_id` or `template_name` or agent name.
    pub async fn clone_upgrade(
        &self,
        name: &str,
        version: Option<&str>,
    ) -> Result<String, String> {
        let entry = self
            .registry
            .find_by_name(name)
            .ok_or_else(|| format!("Agent '{name}' not found"))?;

        let cs = entry.manifest.clone_source.clone();
        let (template_name, hub_template_id, auto_upgrade) = match cs {
            Some(ref c) => {
                let tname = if c.template_name.is_empty() {
                    name.to_string()
                } else {
                    c.template_name.clone()
                };
                (tname, c.hub_template_id.clone(), c.auto_upgrade)
            }
            None => {
                // Allow upgrade by agent name even without clone_source (link after)
                (name.to_string(), None, false)
            }
        };

        let workspace_str = self
            .resolve_agent_workspace(name)
            .ok_or_else(|| format!("Agent '{name}' has no workspace"))?;
        let workspace = std::path::Path::new(&workspace_str);

        let hub_url = self.config.hub.url.trim().to_string();
        if hub_url.is_empty() {
            return Err("Hub URL not configured ([hub] url)".to_string());
        }
        let api_key = clone::hub::read_api_key(&self.config.hub.api_key_env)
            .map_err(|e| e.to_string())?;

        let remote_version = clone::hub::upgrade_workspace_from_hub(
            &hub_url,
            &api_key,
            &template_name,
            version,
            workspace,
            hub_template_id.as_deref(),
        )
        .await
        .map_err(|e| format!("Hub upgrade failed: {e}"))?;

        // Preserve auto_upgrade flag; re-read agent.toml into registry
        if let Ok(toml_str) = std::fs::read_to_string(workspace.join("agent.toml")) {
            if let Ok(mut m) = toml::from_str::<types::agent::AgentManifest>(&toml_str) {
                let _drift = types::serde_compat::take_lenient_diagnostics();
                if !_drift.is_empty() {
                    tracing::warn!(agent = %m.name, count = _drift.len(), details = ?_drift, "agent.toml fields fell back to empty defaults due to type drift — check tool_blocklist/tool_allowlist");
                }
                if let Some(ref mut new_cs) = m.clone_source {
                    new_cs.auto_upgrade = auto_upgrade;
                }
                if let Ok(s) = toml::to_string_pretty(&m) {
                    let _ = std::fs::write(workspace.join("agent.toml"), s);
                }
                if let Some(cs) = m.clone_source {
                    let _ = self.registry.update_clone_source(entry.id, cs);
                }
            }
        }

        let _ = self.restart_agent(&entry.id.to_string());

        info!(
            agent = %name,
            new_version = %remote_version,
            "Clone upgraded from DupHub (definition-layer only)"
        );

        Ok(remote_version)
    }
}
