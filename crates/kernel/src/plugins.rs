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

    /// Upgrade a clone agent from hub by downloading latest .agx, extracting to
    /// a staging directory, then selectively replacing template files while
    /// preserving user data (memory/, sessions/, logs/, users/, data/).
    ///
    /// Files replaced: SOUL.md, system_prompt.md, profile.md, EVOLUTION.md,
    ///                 template.json, skills/, agents/, knowledge/, style/, MEMORY.md
    /// Files preserved: memory/, sessions/, history/, logs/, users/, data/
    pub async fn clone_upgrade(&self, name: &str) -> Result<String, String> {
        use clone::{build_manifest_from_workspace, extract_agx};

        // 1. Find the agent and validate it's a hub clone
        let entry = self
            .registry
            .find_by_name(name)
            .ok_or_else(|| format!("Agent '{}' not found", name))?;

        let cs = entry
            .manifest
            .clone_source
            .as_ref()
            .ok_or_else(|| format!("Agent '{}' is not a clone", name))?;

        let hub_template_id = cs
            .hub_template_id
            .as_ref()
            .ok_or_else(|| format!("Agent '{}' has no hub_template_id", name))?
            .clone();

        let workspace_str = self
            .resolve_agent_workspace(name)
            .ok_or_else(|| format!("Agent '{}' has no workspace", name))?;
        let workspace = std::path::Path::new(&workspace_str);

        // 2. Download latest .agx from hub
        let hub_url = self.config.hub.url.trim_end_matches('/').to_string();
        let download_url = format!(
            "{}/api/templates/{}/download",
            hub_url,
            urlencoding::encode(&hub_template_id)
        );

        // SECURITY: SSRF check before downloading from hub
        types::ssrf::check_ssrf(&download_url)
            .map_err(|e| format!("Hub download URL failed SSRF check: {e}"))?;

        let resp = reqwest::get(&download_url)
            .await
            .map_err(|e| format!("Failed to download from hub: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Hub download failed {}: {} — {}",
                hub_template_id, status, body
            ));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Failed to read response: {e}"))?;

        // 3. Extract to staging directory
        let staging_dir =
            std::env::temp_dir().join(format!("carrier-upgrade-{}", uuid::Uuid::new_v4()));
        extract_agx(&bytes, &staging_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging_dir);
            format!("Failed to extract .agx: {e}")
        })?;

        // 4. Get remote version from staging template.json
        let remote_version: String = std::fs::read_to_string(staging_dir.join("template.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<clone::TemplateManifest>(&s).ok())
            .map(|t| t.version)
            .unwrap_or_default();

        // 5. Selectively replace template files (preserve user data)
        let template_files = [
            "SOUL.md",
            "system_prompt.md",
            "profile.md",
            "EVOLUTION.md",
            "MEMORY.md",
            "template.json",
        ];
        for filename in &template_files {
            let src = staging_dir.join(filename);
            if src.exists() {
                std::fs::copy(&src, workspace.join(filename))
                    .map_err(|e| format!("Failed to copy {}: {e}", filename))?;
            }
        }

        // Replace template directories (remove old, copy new)
        let template_dirs = ["skills", "agents", "knowledge", "style"];
        for dir_name in &template_dirs {
            let workspace_subdir = workspace.join(dir_name);
            let staging_subdir = staging_dir.join(dir_name);
            if workspace_subdir.exists() {
                let _ = std::fs::remove_dir_all(&workspace_subdir);
            }
            if staging_subdir.exists() {
                copy_dir_recursive(&staging_subdir, &workspace_subdir)
                    .map_err(|e| format!("Failed to copy {} dir: {e}", dir_name))?;
            }
        }

        // Clean up staging
        let _ = std::fs::remove_dir_all(&staging_dir);

        // 6. Build new manifest from workspace and write agent.toml
        let mut new_manifest =
            build_manifest_from_workspace(workspace, name, Some(hub_template_id.clone()))
                .map_err(|e| format!("Failed to build manifest: {e}"))?;

        // Preserve the original clone_source but update agx_version
        if let Some(ref mut orig_cs) = new_manifest.clone_source {
            orig_cs.agx_version = if remote_version.is_empty() {
                let current: i64 = cs.agx_version.parse().unwrap_or(0);
                (current + 1).to_string()
            } else {
                remote_version.clone()
            };
            orig_cs.auto_upgrade = cs.auto_upgrade;
        }

        let toml_str = toml::to_string_pretty(&new_manifest)
            .map_err(|e| format!("Failed to serialize agent.toml: {e}"))?;
        std::fs::write(workspace.join("agent.toml"), toml_str)
            .map_err(|e| format!("Failed to write agent.toml: {e}"))?;

        // 7. Update registry entry
        if let Some(updated_cs) = new_manifest.clone_source.clone() {
            self.registry
                .update_clone_source(entry.id, updated_cs)
                .map_err(|e| format!("Failed to update registry: {e}"))?;
        }

        // 8. Restart the agent so it picks up new files
        let _ = self.restart_agent(&entry.id.to_string());

        info!(
            agent = %name,
            new_version = %if remote_version.is_empty() { "bumped" } else { &remote_version },
            "Clone upgraded from hub (v3 extract flow)"
        );

        Ok(if remote_version.is_empty() {
            "upgraded".to_string()
        } else {
            remote_version
        })
    }
}

/// Recursively copy a directory from src to dst.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
