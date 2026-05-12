//! Sender-based routing — dispatches inbound messages to agents by sender_id.
//!
//! Directory structure:
//!   ~/.opencarrier/senders/{sender_id}/config.json   — default routing config
//!   ~/.opencarrier/senders/{sender_id}/aliases.json   — name → agent_id mappings
//!   ~/.opencarrier/senders/{sender_id}/{agent_id}/    — per-sender per-agent session data
//!
//! New senders are auto-assigned to the first available agent.
//! Each sender can give agents custom names (aliases) for natural-language routing.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{info, warn};

/// Per-sender routing config (default agent).
#[derive(Serialize, Deserialize)]
struct SenderConfig {
    agent_id: String,
    created_at: i64,
}

/// Per-sender alias mappings persisted to disk.
#[derive(Serialize, Deserialize, Default)]
struct AliasMap {
    aliases: HashMap<String, String>,
}

/// Per-sender routing: sender_id → agent_id, with name-based aliases.
///
/// Thread-safe via DashMap. Shared between the bridge (reads) and API routes (writes).
pub struct SenderRouter {
    /// In-memory cache: sender_id → agent_id (default route).
    routes: DashMap<String, String>,
    /// In-memory cache: sender_id → { name → agent_id } aliases.
    aliases: DashMap<String, HashMap<String, String>>,
    /// Root directory: ~/.opencarrier/senders/
    senders_dir: PathBuf,
    /// First available agent (for auto-assigning new senders).
    first_agent: Mutex<Option<String>>,
}

impl SenderRouter {
    pub fn new(home_dir: &Path) -> Self {
        let senders_dir = home_dir.join("senders");
        let router = Self {
            routes: DashMap::new(),
            aliases: DashMap::new(),
            senders_dir,
            first_agent: Mutex::new(None),
        };
        router.load_all_from_disk();
        router
    }

    /// Set the first available agent (called after bindings are populated).
    pub fn set_first_agent(&self, agent_id: String) {
        let mut first = self.first_agent.lock().unwrap();
        if first.is_none() {
            info!(agent = %agent_id, "SenderRouter: first agent set");
            *first = Some(agent_id);
        }
    }

    /// Load all existing sender configs from disk into memory.
    fn load_all_from_disk(&self) {
        if !self.senders_dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&self.senders_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let sender_id = entry.file_name().to_string_lossy().to_string();
            let sender_dir = entry.path();

            // Load default route
            let config_path = sender_dir.join("config.json");
            if config_path.exists() {
                match std::fs::read_to_string(&config_path) {
                    Ok(content) => match serde_json::from_str::<SenderConfig>(&content) {
                        Ok(config) => {
                            self.routes
                                .insert(sender_id.clone(), config.agent_id.clone());
                            info!(sender = %sender_id, agent = %config.agent_id, "Loaded sender route");
                        }
                        Err(e) => warn!("Failed to parse sender config: {e}"),
                    },
                    Err(e) => warn!("Failed to read sender config: {e}"),
                }
            }

            // Load aliases
            let alias_path = sender_dir.join("aliases.json");
            if alias_path.exists() {
                match std::fs::read_to_string(&alias_path) {
                    Ok(content) => match serde_json::from_str::<AliasMap>(&content) {
                        Ok(alias_map) => {
                            if !alias_map.aliases.is_empty() {
                                info!(
                                    sender = %sender_id,
                                    count = alias_map.aliases.len(),
                                    "Loaded sender aliases"
                                );
                                self.aliases.insert(sender_id.clone(), alias_map.aliases);
                            }
                        }
                        Err(e) => warn!("Failed to parse sender aliases: {e}"),
                    },
                    Err(e) => warn!("Failed to read sender aliases: {e}"),
                }
            }
        }
        info!(count = self.routes.len(), "Loaded sender routes from disk");
    }

    /// Resolve which agent handles a sender. Auto-assigns first agent if new.
    pub fn resolve(&self, sender_id: &str) -> Option<String> {
        // Check in-memory cache
        if let Some(route) = self.routes.get(sender_id) {
            return Some(route.value().clone());
        }

        // Try loading from disk
        if let Some(agent_id) = self.load_sender_config(sender_id) {
            return Some(agent_id);
        }

        // Auto-assign to first agent
        self.auto_assign(sender_id)
    }

    fn load_sender_config(&self, sender_id: &str) -> Option<String> {
        let config_path = self.senders_dir.join(sender_id).join("config.json");
        if !config_path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&config_path).ok()?;
        let config: SenderConfig = serde_json::from_str(&content).ok()?;
        self.routes
            .insert(sender_id.to_string(), config.agent_id.clone());
        Some(config.agent_id)
    }

    fn auto_assign(&self, sender_id: &str) -> Option<String> {
        let agent_id = {
            let first = self.first_agent.lock().unwrap();
            first.clone()?
        };

        self.persist_route(sender_id, &agent_id);
        self.routes
            .insert(sender_id.to_string(), agent_id.clone());
        info!(sender = %sender_id, agent = %agent_id, "Auto-assigned sender to agent");
        Some(agent_id)
    }

    /// Write a sender's route config and create directory structure.
    fn persist_route(&self, sender_id: &str, agent_id: &str) {
        let sender_dir = self.senders_dir.join(sender_id);
        if let Err(e) = std::fs::create_dir_all(&sender_dir) {
            warn!(sender = %sender_id, "Failed to create sender dir: {e}");
        }

        // Create per-agent directory
        let agent_dir = sender_dir.join(agent_id);
        if let Err(e) = std::fs::create_dir_all(&agent_dir) {
            warn!(sender = %sender_id, agent = %agent_id, "Failed to create sender/agent dir: {e}");
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let config = SenderConfig {
            agent_id: agent_id.to_string(),
            created_at: now,
        };
        let config_path = sender_dir.join("config.json");
        if let Ok(json) = serde_json::to_string_pretty(&config) {
            if let Err(e) = std::fs::write(&config_path, json) {
                warn!(sender = %sender_id, "Failed to write sender config: {e}");
            }
        }
    }

    /// Explicitly set the route for a sender (e.g., user switches agent).
    pub fn set_route(&self, sender_id: &str, agent_id: &str) {
        self.persist_route(sender_id, agent_id);
        self.routes
            .insert(sender_id.to_string(), agent_id.to_string());
        info!(sender = %sender_id, agent = %agent_id, "Sender route updated");
    }

    /// Remove a sender's route from memory and delete config from disk.
    pub fn remove_route(&self, sender_id: &str) -> Option<String> {
        let removed = self.routes.remove(sender_id).map(|(_, v)| v);
        if removed.is_some() {
            let config_path = self.senders_dir.join(sender_id).join("config.json");
            let _ = std::fs::remove_file(&config_path);
        }
        removed
    }

    /// Get a sender's route without triggering auto-assign.
    pub fn get_route(&self, sender_id: &str) -> Option<String> {
        if let Some(route) = self.routes.get(sender_id) {
            return Some(route.value().clone());
        }
        self.load_sender_config(sender_id)
    }

    /// List all sender routes.
    pub fn list_routes(&self) -> Vec<(String, String)> {
        self.routes
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Alias (name) support
    // -----------------------------------------------------------------------

    /// Set an alias for an agent under a sender's namespace.
    /// Persists to aliases.json on disk.
    pub fn set_alias(&self, sender_id: &str, name: &str, agent_id: &str) {
        let name_lower = name.to_lowercase();

        // Update in-memory
        self.aliases
            .entry(sender_id.to_string())
            .or_default()
            .insert(name_lower.clone(), agent_id.to_string());

        // Persist to disk
        self.persist_aliases(sender_id);

        info!(
            sender = %sender_id,
            name = %name,
            agent = %agent_id,
            "Sender alias set"
        );
    }

    /// Resolve an agent by alias name for a given sender.
    /// Name matching is case-insensitive.
    pub fn resolve_by_name(&self, sender_id: &str, name: &str) -> Option<String> {
        let name_lower = name.to_lowercase();

        // Check in-memory
        if let Some(aliases) = self.aliases.get(sender_id) {
            if let Some(agent_id) = aliases.get(&name_lower) {
                return Some(agent_id.clone());
            }
        }

        // Try loading from disk
        self.load_aliases(sender_id);
        if let Some(aliases) = self.aliases.get(sender_id) {
            aliases.get(&name_lower).cloned()
        } else {
            None
        }
    }

    /// Get the alias (name) for a specific agent under a sender.
    pub fn get_alias(&self, sender_id: &str, agent_id: &str) -> Option<String> {
        if let Some(aliases) = self.aliases.get(sender_id) {
            for (name, aid) in aliases.iter() {
                if aid == agent_id {
                    return Some(name.clone());
                }
            }
        }
        None
    }

    /// Check if a sender has any aliases set.
    pub fn has_aliases(&self, sender_id: &str) -> bool {
        if let Some(aliases) = self.aliases.get(sender_id) {
            !aliases.is_empty()
        } else {
            false
        }
    }

    /// Check if a sender's default agent has an alias.
    /// Returns true if the sender has a route but no alias for it.
    pub fn needs_naming(&self, sender_id: &str) -> bool {
        if let Some(agent_id) = self.get_route(sender_id) {
            self.get_alias(sender_id, &agent_id).is_none()
        } else {
            false
        }
    }

    /// List all aliases for a sender as (name, agent_id) pairs.
    pub fn list_aliases(&self, sender_id: &str) -> Vec<(String, String)> {
        if let Some(aliases) = self.aliases.get(sender_id) {
            aliases.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        } else {
            Vec::new()
        }
    }

    /// Load aliases from disk for a sender.
    fn load_aliases(&self, sender_id: &str) {
        let alias_path = self.senders_dir.join(sender_id).join("aliases.json");
        if !alias_path.exists() {
            return;
        }
        let content = match std::fs::read_to_string(&alias_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let alias_map: AliasMap = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => return,
        };
        if !alias_map.aliases.is_empty() {
            self.aliases.insert(sender_id.to_string(), alias_map.aliases);
        }
    }

    /// Persist aliases to disk for a sender.
    fn persist_aliases(&self, sender_id: &str) {
        let sender_dir = self.senders_dir.join(sender_id);
        if let Err(e) = std::fs::create_dir_all(&sender_dir) {
            warn!(sender = %sender_id, "Failed to create sender dir: {e}");
            return;
        }

        let aliases = self.aliases.get(sender_id);
        let alias_map = AliasMap {
            aliases: aliases.map(|a| a.value().clone()).unwrap_or_default(),
        };

        let alias_path = sender_dir.join("aliases.json");
        match serde_json::to_string_pretty(&alias_map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&alias_path, json) {
                    warn!(sender = %sender_id, "Failed to write sender aliases: {e}");
                }
            }
            Err(e) => {
                warn!(sender = %sender_id, "Failed to serialize sender aliases: {e}");
            }
        }
    }
}
