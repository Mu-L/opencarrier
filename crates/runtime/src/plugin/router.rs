//! Sender-based routing — dispatches inbound messages to agents by sender_id.
//!
//! Directory structure:
//!   ~/.opencarrier/senders/{sender_id}/config.json   — routing + clone registry
//!
//! config.json format:
//!   {
//!     "default": "<agent_name>",        // agent name (not UUID)
//!     "clones": {
//!       "<agent_name>": { "alias": "名字", "installed_at": 1778713691 },
//!       ...
//!     },
//!     "created_at": 1778389219
//!   }
//!
//! Legacy formats (auto-migrated on load):
//!   - Old unified: { "default": "<uuid>", ... }  → migrated via migrate_uuid_to_names()
//!   - Original: { "agent_id": "<uuid>", ... } + aliases.json

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

/// Per-sender routing: sender_id → agent_name, with name-based aliases.
///
/// Thread-safe via DashMap. Shared between the bridge (reads) and API routes (writes).
pub struct SenderRouter {
    /// In-memory cache: sender_id → agent_name (default route).
    routes: DashMap<String, String>,
    /// In-memory cache: sender_id → { agent_name → CloneEntry }.
    clones: DashMap<String, HashMap<String, CloneEntry>>,
    /// Root directory: ~/.opencarrier/senders/
    senders_dir: PathBuf,
    /// First available agent name (for auto-assigning new senders).
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
    pub fn set_first_agent(&self, agent_name: String) {
        let mut first = self.first_agent.lock().unwrap_or_else(|e| e.into_inner());
        if first.is_none() {
            info!(agent = %agent_name, "SenderRouter: first agent set");
            *first = Some(agent_name);
        }
    }

    /// Migrate any routes still stored as UUIDs to agent names.
    /// Called once after the kernel registry is populated (agents are spawned).
    /// `lookup` is a closure that maps UUID → agent_name.
    pub fn migrate_uuid_to_names<F>(&self, lookup: F)
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut migrated = 0usize;

        // Collect routes that need migration
        let to_migrate: Vec<(String, String)> = self.routes
            .iter()
            .filter_map(|entry| {
                let val = entry.value();
                if uuid::Uuid::parse_str(val).is_ok() {
                    lookup(val).map(|name| (entry.key().clone(), name))
                } else {
                    None
                }
            })
            .collect();

        for (sender_id, new_name) in &to_migrate {
            self.routes.insert(sender_id.clone(), new_name.clone());
            migrated += 1;
        }

        // Also migrate clones map keys
        let clone_migrations: Vec<(String, Vec<(String, String)>)> = self.clones
            .iter()
            .filter_map(|entry| {
                let clones_map = entry.value();
                let keys: Vec<(String, String)> = clones_map
                    .keys()
                    .filter_map(|k| {
                        if uuid::Uuid::parse_str(k).is_ok() {
                            lookup(k).map(|name| (k.clone(), name))
                        } else {
                            None
                        }
                    })
                    .collect();
                if keys.is_empty() { None } else { Some((entry.key().clone(), keys)) }
            })
            .collect();

        for (sender_id, keys) in &clone_migrations {
            if let Some(mut clones_map) = self.clones.get_mut(sender_id) {
                for (old_key, new_key) in keys {
                    if let Some(entry_val) = clones_map.remove(old_key) {
                        clones_map.insert(new_key.clone(), entry_val);
                    }
                }
            }
        }

        // Persist migrated configs
        if migrated > 0 {
            for (sender_id, _) in &to_migrate {
                self.persist_config(sender_id);
            }
            // Check for orphaned routes (UUID that couldn't be resolved)
            let orphaned = self.routes
                .iter()
                .filter(|e| uuid::Uuid::parse_str(e.value()).is_ok())
                .count();
            info!(migrated, orphaned, "Migrated sender routes from UUID to agent name");
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
        let agent_name = {
            let first = self.first_agent.lock().unwrap_or_else(|e| e.into_inner());
            first.clone()?
        };

        if let Err(e) = self.persist_route(sender_id, &agent_name) {
            warn!(sender = %sender_id, "Failed to persist auto-assign: {e}");
            return None;
        }
        self.routes
            .insert(sender_id.to_string(), agent_name.clone());
        info!(sender = %sender_id, agent = %agent_name, "Auto-assigned sender to agent");
        Some(agent_name)
    }

    /// Write a sender's route config and create directory structure.
    /// Adds the agent to clones if not already present.
    fn persist_route(&self, sender_id: &str, agent_id: &str) -> Result<(), String> {
        let sender_dir = self.senders_dir.join(sender_id);
        if let Err(e) = std::fs::create_dir_all(&sender_dir) {
            return Err(format!("Failed to create sender dir: {e}"));
        }

        // Create per-agent directory
        let agent_dir = sender_dir.join(agent_id);
        if let Err(e) = std::fs::create_dir_all(&agent_dir) {
            return Err(format!("Failed to create sender/agent dir: {e}"));
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
        self.write_config_atomic(sender_id, &config)
    }

    /// Persist the full sender config to disk (atomic write).
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
        if let Err(e) = self.write_config_atomic(sender_id, &config) {
            warn!(sender = %sender_id, "Failed to write sender config: {e}");
        }
    }

    /// Atomic write: write to temp file then rename (crash-safe on POSIX).
    fn write_config_atomic(&self, sender_id: &str, config: &SenderConfig) -> Result<(), String> {
        let config_path = self.senders_dir.join(sender_id).join("config.json");
        let json = serde_json::to_string_pretty(config).map_err(|e| format!("Serialize error: {e}"))?;
        let tmp_path = config_path.with_extension("tmp");
        std::fs::write(&tmp_path, &json).map_err(|e| format!("Write error: {e}"))?;
        std::fs::rename(&tmp_path, &config_path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            format!("Rename error: {e}")
        })
    }

    /// Explicitly set the route for a sender (e.g., user switches agent).
    /// Rolls back in-memory state if disk write fails.
    pub fn set_route(&self, sender_id: &str, agent_id: &str) {
        if let Err(e) = self.persist_route(sender_id, agent_id) {
            warn!(sender = %sender_id, "Failed to persist route, skipping in-memory update: {e}");
            return;
        }
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

    /// List all routes with the user-set alias for each (sender_id, agent, alias).
    /// alias is None when the sender has no clone entry for that agent.
    pub fn list_routes_with_aliases(&self) -> Vec<(String, String, Option<String>)> {
        self.routes
            .iter()
            .map(|r| {
                let alias = self
                    .clones
                    .get(r.key())
                    .and_then(|m| m.get(r.value()).map(|e| e.alias.clone()));
                (r.key().clone(), r.value().clone(), alias)
            })
            .collect()
    }

    /// Count how many senders have each agent bound (default + clones).
    /// Returns agent_id → sender_count.
    pub fn count_agents_per_sender(&self) -> HashMap<String, usize> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in self.routes.iter() {
            let sender_id = entry.key();
            let default_agent = entry.value();
            // Count default
            *counts.entry(default_agent.clone()).or_insert(0) += 1;
            // Count clones (skip default since already counted)
            if let Some(clones_map) = self.clones.get(sender_id) {
                for agent_id in clones_map.keys() {
                    if agent_id != default_agent {
                        *counts.entry(agent_id.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
        counts
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
