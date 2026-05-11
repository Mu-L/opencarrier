//! Plugin dependency resolution and clone upgrade management.
//!
//! Handles downloading missing plugins from Hub and upgrading clone agents
//! with the latest template files while preserving user data.

use crate::kernel::CarrierKernel;
use carrier_runtime::kernel_handle::KernelHandle;
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
            if carrier_clone::hub::is_plugin_installed(&plugins_dir, plugin_name) {
                tracing::info!(plugin = %plugin_name, "Plugin already installed, skipping");
                continue;
            }

            tracing::info!(plugin = %plugin_name, "Downloading missing plugin from Hub...");
            match carrier_clone::hub::install_plugin(
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

    /// Upgrade a clone agent from hub by downloading latest .agx and selectively
    /// replacing template files while preserving user data.
    ///
    /// Files replaced: agent.toml, SOUL.md, system_prompt.md, profile.md,
    ///                 skills/, agents/, EVOLUTION.md, knowledge/
    /// Files preserved: memory/, sessions/, logs/, users/, data/ (except knowledge/)
    pub async fn clone_upgrade(&self, name: &str) -> Result<String, String> {
        use carrier_clone::{convert_to_manifest, load_agx};

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

        // 3. Parse the .agx
        let tmp_dir =
            std::env::temp_dir().join(format!("carrier-upgrade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("Failed to create temp dir: {e}"))?;
        let tmp_path = tmp_dir.join("upgrade.agx");
        std::fs::write(&tmp_path, &bytes).map_err(|e| format!("Failed to write temp file: {e}"))?;

        let clone_data = load_agx(&tmp_path).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            format!("Failed to parse .agx: {e}")
        })?;
        let _ = std::fs::remove_dir_all(&tmp_dir);

        // 4. Get the remote version from clone_data manifest
        let remote_version: String = clone_data
            .manifest
            .as_ref()
            .map(|m| m.version.clone())
            .unwrap_or_default();

        // 5. Selectively replace template files

        // Write SOUL.md
        if !clone_data.soul.is_empty() {
            std::fs::write(workspace.join("SOUL.md"), &clone_data.soul)
                .map_err(|e| format!("Failed to write SOUL.md: {e}"))?;
        }

        // Write system_prompt.md
        if !clone_data.system_prompt.is_empty() {
            std::fs::write(
                workspace.join("system_prompt.md"),
                &clone_data.system_prompt,
            )
            .map_err(|e| format!("Failed to write system_prompt.md: {e}"))?;
        }

        // Write profile.md
        if !clone_data.profile.is_empty() {
            std::fs::write(workspace.join("profile.md"), &clone_data.profile)
                .map_err(|e| format!("Failed to write profile.md: {e}"))?;
        }

        // Write EVOLUTION.md
        if !clone_data.evolution.is_empty() {
            std::fs::write(workspace.join("EVOLUTION.md"), &clone_data.evolution)
                .map_err(|e| format!("Failed to write EVOLUTION.md: {e}"))?;
        }

        // Replace skills/ directory
        let skills_dir = workspace.join("skills");
        if skills_dir.exists() {
            let _ = std::fs::remove_dir_all(&skills_dir);
        }
        std::fs::create_dir_all(&skills_dir)
            .map_err(|e| format!("Failed to create skills dir: {e}"))?;
        for skill in &clone_data.skills {
            let skill_dir = skills_dir.join(&skill.name);
            std::fs::create_dir_all(&skill_dir)
                .map_err(|e| format!("Failed to create skill dir: {e}"))?;

            let tools_str = if skill.allowed_tools.is_empty() {
                String::new()
            } else {
                format!(
                    "\nallowed_tools: {}",
                    carrier_clone::format_string_array(&skill.allowed_tools)
                )
            };
            let skill_md = format!(
                "---\nname: {}\nwhen_to_use: {}{}\n---\n\n{}",
                skill.name, skill.when_to_use, tools_str, skill.prompt
            );
            std::fs::write(skill_dir.join("SKILL.md"), skill_md)
                .map_err(|e| format!("Failed to write skill: {e}"))?;

            if !skill.scripts.is_empty() {
                let scripts_dir = skill_dir.join("scripts");
                std::fs::create_dir_all(&scripts_dir)
                    .map_err(|e| format!("Failed to create scripts dir: {e}"))?;
                for script in &skill.scripts {
                    std::fs::write(
                        scripts_dir.join(format!("{}.toml", script.name)),
                        &script.toml_content,
                    )
                    .map_err(|e| format!("Failed to write script: {e}"))?;
                }
            }
        }

        // Replace agents/ directory
        let agents_dir = workspace.join("agents");
        if agents_dir.exists() {
            let _ = std::fs::remove_dir_all(&agents_dir);
        }
        std::fs::create_dir_all(&agents_dir)
            .map_err(|e| format!("Failed to create agents dir: {e}"))?;
        for agent in &clone_data.agents {
            let color_line = agent
                .color
                .as_ref()
                .map(|c| format!("color: {}", c))
                .unwrap_or_default();
            let tools_line = if agent.tools.is_empty() {
                String::new()
            } else {
                format!(
                    "\ntools: {}",
                    carrier_clone::format_string_array(&agent.tools)
                )
            };
            let model_line = if agent.model.is_empty() {
                String::new()
            } else {
                format!("\nmodel: {}", agent.model)
            };
            let agent_md = format!(
                "---\nname: {}\ndescription: {}{}{}\n{}\n---\n\n{}",
                agent.name, agent.description, tools_line, model_line, color_line, agent.prompt,
            );
            std::fs::write(agents_dir.join(format!("{}.md", agent.name)), agent_md)
                .map_err(|e| format!("Failed to write agent: {e}"))?;
        }

        // Replace knowledge/ files
        let knowledge_dir = workspace.join("knowledge");
        std::fs::create_dir_all(&knowledge_dir)
            .map_err(|e| format!("Failed to create knowledge dir: {e}"))?;
        for (kname, content) in &clone_data.knowledge {
            std::fs::write(knowledge_dir.join(kname), content)
                .map_err(|e| format!("Failed to write knowledge: {e}"))?;
        }

        // 6. Write new agent.toml preserving clone_source with updated version
        let mut new_manifest = convert_to_manifest(&clone_data, Some(hub_template_id.clone()));
        new_manifest.name = name.to_string();
        new_manifest.workspace = Some(workspace.to_path_buf());

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
        let updated_cs = new_manifest.clone_source.clone().unwrap();
        self.registry
            .update_clone_source(entry.id, updated_cs)
            .map_err(|e| format!("Failed to update registry: {e}"))?;

        // 8. Restart the agent so it picks up new files
        let _ = self.restart_agent(&entry.id.to_string());

        info!(
            agent = %name,
            new_version = %if remote_version.is_empty() { "bumped" } else { &remote_version },
            "Clone upgraded from hub"
        );

        Ok(if remote_version.is_empty() {
            "upgraded".to_string()
        } else {
            remote_version
        })
    }
}
