//! Sender-based routing — dispatches inbound messages to agents by sender_id.
//!
//! Directory structure:
//!   ~/.opencarrier/senders/{sender_id}/config.json   — routing + clone registry
//!
//! config.json format (unified):
//!   {
//!     "default": "<agent_id>",          // current active clone
//!     "clones": {
//!       "<agent_id>": { "alias": "名字", "installed_at": 1778713691 },
//!       ...
//!     },
//!     "created_at": 1778389219
//!   }
//!
//! Legacy format (auto-migrated on first load):
//!   { "agent_id": "...", "created_at": ... }  +  aliases.json

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{info, warn};

/// A clone bound to a sender.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CloneEntry {
    pub alias: String,
    pub installed_at: i64,
}

/// Unified sender config: default route + all bound clones.
#[derive(Serialize, Deserialize, Debug)]
struct SenderConfig {
    default: String,
    clones: HashMap<String, CloneEntry>,
    created_at: i64,
}

/// Legacy format for migration.
#[derive(Deserialize)]
struct LegacySenderConfig {
    agent_id: String,
    created_at: i64,
}

/// Legacy alias map for migration.
#[derive(Deserialize, Default)]
struct LegacyAliasMap {
    aliases: HashMap<String, String>,
}

/// Per-sender routing: sender_id → agent_id, with name-based aliases.
///
/// Thread-safe via DashMap. Shared between the bridge (reads) and API routes (writes).
pub struct SenderRouter {
    /// In-memory cache: sender_id → agent_id (default route).
    routes: DashMap<String, String>,
    /// In-memory cache: sender_id → { agent_id → CloneEntry }.
    clones: DashMap<String, HashMap<String, CloneEntry>>,
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
            clones: DashMap::new(),
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
    /// Handles both new and legacy formats, migrating on the fly.
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

            let config_path = sender_dir.join("config.json");
            if !config_path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(sender = %sender_id, "Failed to read config: {e}");
                    continue;
                }
            };

            // Try new format first
            if let Ok(config) = serde_json::from_str::<SenderConfig>(&content) {
                self.routes.insert(sender_id.clone(), config.default);
                self.clones.insert(sender_id.clone(), config.clones);
                info!(sender = %sender_id, default = ?self.routes.get(&sender_id).map(|r| r.value().clone()), clones = ?self.clones.get(&sender_id).map(|c| c.len()), "Loaded sender config (new format)");
                continue;
            }

            // Fall back to legacy format
            let legacy: LegacySenderConfig = match serde_json::from_str(&content) {
                Ok(c) => c,
                Err(e) => {
                    warn!(sender = %sender_id, "Failed to parse config: {e}");
                    continue;
                }
            };

            self.routes
                .insert(sender_id.clone(), legacy.agent_id.clone());

            // Merge aliases.json into clones map
            let mut clones_map = HashMap::new();
            let alias_path = sender_dir.join("aliases.json");
            if alias_path.exists() {
                if let Ok(alias_content) = std::fs::read_to_string(&alias_path) {
                    if let Ok(alias_map) = serde_json::from_str::<LegacyAliasMap>(&alias_content) {
                        for (name, agent_id) in alias_map.aliases {
                            clones_map.insert(
                                agent_id,
                                CloneEntry {
                                    alias: name,
                                    installed_at: legacy.created_at,
                                },
                            );
                        }
                    }
                }
            }
            // Ensure the default agent is in clones
            if !clones_map.contains_key(&legacy.agent_id) {
                clones_map.insert(
                    legacy.agent_id.clone(),
                    CloneEntry {
                        alias: String::new(),
                        installed_at: legacy.created_at,
                    },
                );
            }

            info!(
                sender = %sender_id,
                default = %legacy.agent_id,
                clones = clones_map.len(),
                "Migrated sender config from legacy format"
            );
            self.clones.insert(sender_id.clone(), clones_map);
            self.routes
                .insert(sender_id.clone(), legacy.agent_id.clone());

            // Persist in new format and remove old aliases.json
            self.persist_config(&sender_id);
            let _ = std::fs::remove_file(&alias_path);
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
            .insert(sender_id.to_string(), config.default.clone());
        self.clones
            .insert(sender_id.to_string(), config.clones);
        Some(config.default)
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
    /// Adds the agent to clones if not already present.
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

        // Add to clones if not present
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut clones_map = self
            .clones
            .entry(sender_id.to_string())
            .or_default()
            .clone();
        let created_at = clones_map
            .values()
            .map(|e| e.installed_at)
            .min()
            .unwrap_or(now);
        if !clones_map.contains_key(agent_id) {
            clones_map.insert(
                agent_id.to_string(),
                CloneEntry {
                    alias: String::new(),
                    installed_at: now,
                },
            );
            self.clones.insert(sender_id.to_string(), clones_map);
        }

        let config = SenderConfig {
            default: agent_id.to_string(),
            clones: self
                .clones
                .get(sender_id)
                .map(|c| c.value().clone())
                .unwrap_or_default(),
            created_at,
        };
        let config_path = sender_dir.join("config.json");
        if let Ok(json) = serde_json::to_string_pretty(&config) {
            if let Err(e) = std::fs::write(&config_path, json) {
                warn!(sender = %sender_id, "Failed to write sender config: {e}");
            }
        }
    }

    /// Persist the full sender config to disk.
    fn persist_config(&self, sender_id: &str) {
        let sender_dir = self.senders_dir.join(sender_id);
        if let Err(e) = std::fs::create_dir_all(&sender_dir) {
            warn!(sender = %sender_id, "Failed to create sender dir: {e}");
            return;
        }

        let default = self
            .routes
            .get(sender_id)
            .map(|r| r.value().clone())
            .unwrap_or_default();
        let clones = self
            .clones
            .get(sender_id)
            .map(|c| c.value().clone())
            .unwrap_or_default();
        let created_at = clones
            .values()
            .map(|e| e.installed_at)
            .min()
            .unwrap_or(0);

        let config = SenderConfig {
            default,
            clones,
            created_at,
        };
        let config_path = sender_dir.join("config.json");
        match serde_json::to_string_pretty(&config) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&config_path, json) {
                    warn!(sender = %sender_id, "Failed to write sender config: {e}");
                }
            }
            Err(e) => {
                warn!(sender = %sender_id, "Failed to serialize sender config: {e}");
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
            self.clones.remove(sender_id);
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
    // Alias (name) support — backed by clones map
    // -----------------------------------------------------------------------

    /// Set an alias for an agent under a sender's namespace.
    /// Persists to config.json on disk.
    pub fn set_alias(&self, sender_id: &str, name: &str, agent_id: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        {
            let mut clones_map = self
                .clones
                .entry(sender_id.to_string())
                .or_default();
            if let Some(entry) = clones_map.get_mut(agent_id) {
                entry.alias = name.to_string();
            } else {
                clones_map.insert(
                    agent_id.to_string(),
                    CloneEntry {
                        alias: name.to_string(),
                        installed_at: now,
                    },
                );
            }
        } // Drop guard before persist_config to avoid RwLock deadlock

        // Persist to disk
        self.persist_config(sender_id);

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
        if let Some(clones_map) = self.clones.get(sender_id) {
            for (agent_id, entry) in clones_map.iter() {
                if entry.alias.to_lowercase() == name_lower {
                    return Some(agent_id.clone());
                }
            }
        }

        // Try loading from disk
        self.load_sender_config(sender_id);
        if let Some(clones_map) = self.clones.get(sender_id) {
            for (agent_id, entry) in clones_map.iter() {
                if entry.alias.to_lowercase() == name_lower {
                    return Some(agent_id.clone());
                }
            }
        }
        None
    }

    /// Get the alias (name) for a specific agent under a sender.
    pub fn get_alias(&self, sender_id: &str, agent_id: &str) -> Option<String> {
        if let Some(clones_map) = self.clones.get(sender_id) {
            if let Some(entry) = clones_map.get(agent_id) {
                if !entry.alias.is_empty() {
                    return Some(entry.alias.clone());
                }
            }
        }
        None
    }

    /// Check if a sender has any aliases set.
    pub fn has_aliases(&self, sender_id: &str) -> bool {
        if let Some(clones_map) = self.clones.get(sender_id) {
            clones_map.values().any(|e| !e.alias.is_empty())
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

    /// List all aliases for a sender as (alias, agent_id) pairs.
    /// Only returns entries with non-empty aliases.
    pub fn list_aliases(&self, sender_id: &str) -> Vec<(String, String)> {
        if let Some(clones_map) = self.clones.get(sender_id) {
            clones_map
                .iter()
                .filter(|(_, e)| !e.alias.is_empty())
                .map(|(agent_id, e)| (e.alias.clone(), agent_id.clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// List all clones for a sender as (agent_id, CloneEntry) pairs.
    /// Includes entries with and without aliases.
    pub fn list_clones(&self, sender_id: &str) -> Vec<(String, CloneEntry)> {
        if let Some(clones_map) = self.clones.get(sender_id) {
            clones_map
                .iter()
                .map(|(agent_id, e)| (agent_id.clone(), e.clone()))
                .collect()
        } else {
            Vec::new()
        }
    }
}
