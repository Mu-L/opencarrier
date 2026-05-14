//! CarrierKernel — assembles all subsystems and provides the main API.

use crate::background::BackgroundExecutor;
use crate::brain::Brain;
use crate::capabilities::{manifest_to_capabilities, CapabilityManager};
use crate::config::load_config;
use crate::error::{KernelError, KernelResult};
use crate::event_bus::EventBus;
use crate::metering::MeteringEngine;
use crate::registry::AgentRegistry;
use crate::scheduler::AgentScheduler;
use crate::supervisor::Supervisor;
use crate::prompt_sources::read_identity_file;
use crate::workspace::{ensure_workspace, generate_identity_files};
use memory::MemorySubstrate;
use runtime::audit::AuditLog;
use runtime::sandbox::WasmSandbox;
use types::agent::*;
use types::config::KernelConfig;
use types::tool::ToolDefinition;

use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};
use tracing::{info, warn};

/// LLM brain and model catalog subsystem.
pub struct KernelBrain {
    /// The carrier's independent LLM brain. Always loaded — boot fails without a valid brain.json.
    /// Wrapped in RwLock to allow hot-reload of brain.json at runtime.
    pub(crate) brain: Arc<std::sync::RwLock<Arc<Brain>>>,
    /// Path to brain.json (saved at boot for hot-reload writes).
    pub(crate) brain_path: std::path::PathBuf,
    /// Model catalog registry (RwLock for auth status refresh from API).
    pub model_catalog: std::sync::RwLock<runtime::model_catalog::ModelCatalog>,
}

/// A2A (Agent-to-Agent) communication subsystem.
pub struct KernelA2a {
    /// A2A task store for tracking task lifecycle.
    pub a2a_task_store: runtime::a2a::A2aTaskStore,
    /// Discovered external A2A agent cards with discovery timestamp.
    pub a2a_external_agents: std::sync::Mutex<Vec<(String, runtime::a2a::AgentCard, std::time::Instant)>>,
}

impl KernelA2a {
    /// Remove external agent entries that have been stale for longer than the
    /// given TTL. This prevents stale / unreachable agents from accumulating
    /// in the discovery store.
    pub fn cleanup_stale_agents(&self) {
        const STALE_TTL_SECS: u64 = 600; // 10 minutes
        if let Ok(mut agents) = self.a2a_external_agents.lock() {
            let now = std::time::Instant::now();
            agents.retain(|(_, _, discovered_at)| now.duration_since(*discovered_at).as_secs() < STALE_TTL_SECS);
        }
    }
}

/// External service integrations (web search, media, TTS, embeddings).
pub struct KernelServices {
    /// Web tools context (multi-provider search + SSRF-protected fetch + caching).
    pub web_ctx: runtime::web_search::WebToolsContext,
    /// Media understanding engine (image description, audio transcription).
    pub media_engine: runtime::media_understanding::MediaEngine,
}

/// Plugin and MCP tooling subsystem.
pub struct KernelPlugins {
    /// MCP server connections keyed by normalized server name.
    /// DashMap allows concurrent tool calls to different servers without blocking each other.
    pub mcp_connections: dashmap::DashMap<String, runtime::mcp::McpConnection>,
    /// MCP tool definitions cache (populated after connections are established).
    pub mcp_tools: std::sync::Mutex<Vec<ToolDefinition>>,
    /// Toolset registry: name -> tool definitions for that toolset.
    pub toolset_registry: std::sync::RwLock<std::collections::HashMap<String, Vec<ToolDefinition>>>,
    /// Configured MCP server list (from config, used for MCP connections).
    pub effective_mcp_servers: std::sync::RwLock<Vec<types::config::McpServerConfigEntry>>,
    /// Plugin tool dispatcher — routes plugin tool calls to loaded shared libraries.
    pub plugin_tool_dispatcher:
        std::sync::Mutex<Option<Arc<runtime::plugin::tool_dispatch::PluginToolDispatcher>>>,
    /// Per-server consecutive reconnection failure count for exponential backoff.
    /// Key: normalized server name, Value: failure count.
    pub mcp_reconnect_failures: dashmap::DashMap<String, u32>,
}

/// Agent scheduling, supervision, and runtime execution subsystem.
pub struct KernelRuntime {
    /// Agent scheduler.
    pub scheduler: AgentScheduler,
    /// Process supervisor.
    pub supervisor: Supervisor,
    /// Background agent executor.
    pub background: BackgroundExecutor,
    /// Tracks running agent tasks for cancellation support.
    pub running_tasks: dashmap::DashMap<AgentId, tokio::task::AbortHandle>,
    /// WASM sandbox engine (shared across all WASM agent executions).
    pub(crate) wasm_sandbox: WasmSandbox,
    /// Per-agent message locks — serializes LLM calls for the same agent to prevent
    /// session corruption when multiple messages arrive concurrently.
    pub(crate) agent_msg_locks: dashmap::DashMap<AgentId, Arc<tokio::sync::Mutex<()>>>,
    /// File watcher handles for clone agents (stopped when dropped).
    pub(crate) watcher_handles: std::sync::Mutex<Vec<lifecycle::watcher::WatcherHandle>>,
}

/// Cross-cutting coordination: capabilities, events, bindings, hooks, and process management.
pub struct KernelCoordination {
    /// Capability manager.
    pub capabilities: CapabilityManager,
    /// Event bus.
    pub event_bus: EventBus,
    /// Agent bindings for multi-account routing (Mutex for runtime add/remove).
    pub bindings: std::sync::Mutex<Vec<types::config::AgentBinding>>,
    /// Broadcast configuration.
    pub broadcast: types::config::BroadcastConfig,
    /// Plugin lifecycle hook registry.
    pub hooks: runtime::hooks::HookRegistry,
    /// Persistent process manager for interactive sessions (REPLs, servers).
    pub process_manager: Arc<runtime::process_manager::ProcessManager>,
    /// Boot timestamp for uptime calculation.
    pub booted_at: std::time::Instant,
    /// Weak self-reference for trigger dispatch (set after Arc wrapping).
    pub(crate) self_handle: OnceLock<Weak<CarrierKernel>>,
}

/// The main Carrier kernel — coordinates all subsystems.
pub struct CarrierKernel {
    /// Kernel configuration.
    pub config: KernelConfig,
    /// Agent registry.
    pub registry: AgentRegistry,
    /// Memory substrate.
    pub memory: Arc<MemorySubstrate>,
    /// Merkle hash chain audit trail.
    pub audit_log: Arc<AuditLog>,
    /// Cost metering engine.
    pub metering: Arc<MeteringEngine>,
    /// Cron job scheduler.
    pub cron_scheduler: crate::cron::CronScheduler,

    /// LLM brain and model catalog.
    pub brain: KernelBrain,
    /// A2A communication subsystem.
    pub a2a: KernelA2a,
    /// External service integrations.
    pub services: KernelServices,
    /// Plugin and MCP tooling.
    pub plugins: KernelPlugins,
    /// Scheduling, supervision, and runtime.
    pub runtime: KernelRuntime,
    /// Coordination: capabilities, events, bindings, hooks, processes.
    pub coordination: KernelCoordination,
}

// ── Internal boot helpers ──────────────────────────────────

impl CarrierKernel {
    /// Fetch brain configuration from Hub (blocking wrapper).
    fn fetch_brain_from_hub(
        hub: &types::config::HubConfig,
        brain_path: &std::path::Path,
    ) -> Result<types::brain::BrainConfig, String> {
        let api_key = std::env::var(&hub.api_key_env)
            .map_err(|_| format!("Environment variable {} not set", hub.api_key_env))?;

        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("Failed to create tokio runtime: {e}"))?;
        let json_value = rt.block_on(
            clone::hub::fetch_brain_config(&hub.url, &api_key)
        )
        .map_err(|e| format!("Hub brain config fetch failed: {e}"))?;

        let json_str = serde_json::to_string(&json_value)
            .map_err(|e| format!("Failed to serialize brain config: {e}"))?;

        let config: types::brain::BrainConfig = serde_json::from_str(&json_str)
            .map_err(|e| format!("Invalid brain config from Hub: {e}"))?;

        if let Some(parent) = brain_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(brain_path, &json_str)
            .map_err(|e| format!("Failed to write brain.json: {e}"))?;

        Ok(config)
    }

    /// Run post-conversation evolution for clone agents (background, non-blocking).
    ///
    /// Checks if evolution is enabled, the agent is a clone (empty system_prompt),
    /// and the conversation is non-trivial. If so, spawns a background task that:
    /// 1. Calls `should_skip()` for local filtering
    /// 2. Sends the conversation to LLM for analysis
    /// 3. Parses the response and writes knowledge files
    pub fn maybe_run_evolution(
        &self,
        manifest: &types::agent::AgentManifest,
        user_msg: &str,
        response: &str,
        owner_id: Option<&str>,
        sender_id: Option<&str>,
    ) {
        // Check config + clone mode
        if !self.config.clone_lifecycle.evolution_enabled {
            return;
        }
        let Some(ref workspace) = manifest.workspace else {
            return;
        };
        // Clone mode: empty system_prompt signals dynamic assembly
        if !manifest.model.system_prompt.is_empty() {
            return;
        }
        // Check per-clone evolution config (EVOLUTION.md)
        let evo_config = lifecycle::evolution_config::read_evolution_config(workspace);
        let knowledge_count = std::fs::read_dir(workspace.join("knowledge"))
            .map(|d| d.count())
            .unwrap_or(0);
        if !lifecycle::evolution_config::should_evolve(&evo_config, knowledge_count) {
            return;
        }
        // Local pre-filter
        if lifecycle::evolution::should_skip(user_msg, response) {
            return;
        }

        let workspace = workspace.clone();
        let user_msg = user_msg.to_string();
        let response = response.to_string();
        let clone_name = manifest.name.clone();
        let owner_id_owned = owner_id.map(|s| s.to_string());
        let sender_id_owned = sender_id.map(|s| s.to_string());
        let home_dir = self.config.home_dir.clone();
        let feedback_to_hub = evo_config.feedback_to_hub;
        let hub_url = self.config.hub.url.clone();
        let hub_api_key =
            clone::hub::read_api_key(&self.config.hub.api_key_env).unwrap_or_default();
        let driver = match self.resolve_driver(manifest) {
            Ok(d) => d,
            Err(_) => return,
        };
        let memory_md = read_identity_file(&workspace, "MEMORY.md");

        tokio::spawn(async move {
            let prompt = lifecycle::evolution::build_analysis_prompt();
            let memory_index = memory_md.unwrap_or_default();
            let mem_preview = if memory_index.len() > 2000 {
                format!("{}...(省略)", &memory_index[..2000])
            } else {
                memory_index
            };
            let resp_preview = if response.len() > 4000 {
                format!("{}...(截断)", &response[..4000])
            } else {
                response.clone()
            };
            let user_prompt = format!(
                "已知知识索引：\n{}\n\n---\n\n对话：\n用户: {}\n\n助手: {}",
                mem_preview, user_msg, resp_preview
            );

            let request = runtime::llm_driver::CompletionRequest {
                model: String::new(), // driver uses its default
                messages: vec![types::message::Message {
                    role: types::message::Role::User,
                    content: types::message::MessageContent::Text(user_prompt),
                }],
                tools: vec![],
                max_tokens: 2048,
                temperature: 0.3,
                system: Some(prompt),
                thinking: None,
                extra: Default::default(),
            };

            match driver.complete(request).await {
                Ok(completion) => {
                    let text = completion.text();
                    match lifecycle::evolution::parse_analysis_response(&text) {
                        Ok(analysis) => {
                            let saved = lifecycle::evolution::apply_evolution(
                                &workspace, &analysis,
                                owner_id_owned.as_deref(),
                                sender_id_owned.as_deref(),
                                Some(&home_dir),
                            );
                            if !saved.is_empty() {
                                tracing::info!(
                                    count = saved.len(),
                                    "Evolution: new knowledge extracted"
                                );
                            }

                            // Feedback pipeline — anonymize and push to Hub
                            if feedback_to_hub && !analysis.knowledge.is_empty() {
                                for candidate in &analysis.knowledge {
                                    let (sys, user) =
                                        lifecycle::feedback::build_anonymize_prompt(
                                            &candidate.title,
                                            &candidate.content,
                                        );
                                    let anon_req = runtime::llm_driver::CompletionRequest {
                                        model: String::new(),
                                        messages: vec![types::message::Message {
                                            role: types::message::Role::User,
                                            content: types::message::MessageContent::Text(
                                                user,
                                            ),
                                        }],
                                        tools: vec![],
                                        max_tokens: 1024,
                                        temperature: 0.1,
                                        system: Some(sys),
                                        thinking: None,
                                        extra: Default::default(),
                                    };
                                    match driver.complete(anon_req).await {
                                        Ok(anon_resp) => {
                                            let anon_text = anon_resp.text();
                                            let (title, content) =
                                                lifecycle::feedback::parse_anonymize_response(
                                                    &anon_text,
                                                )
                                                .unwrap_or_else(|_| {
                                                    (candidate.title.clone(), candidate.content.clone())
                                                });
                                            if let Err(e) =
                                                lifecycle::feedback::save_feedback(
                                                    &workspace,
                                                    &clone_name,
                                                    &title,
                                                    &content,
                                                )
                                            {
                                                tracing::warn!(error = %e, "Feedback: failed to save");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "Feedback: anonymize LLM failed");
                                        }
                                    }
                                }

                                // Push collected feedback to Hub
                                if let Ok(entries) =
                                    lifecycle::feedback::collect_feedback(&workspace)
                                {
                                    if !entries.is_empty() {
                                        match lifecycle::feedback::push_feedback_to_hub(
                                            &hub_url,
                                            &hub_api_key,
                                            &entries,
                                        )
                                        .await
                                        {
                                            Ok(results) => {
                                                tracing::info!(
                                                    count = results.len(),
                                                    "Feedback: pushed to Hub"
                                                );
                                            }
                                            Err(e) => {
                                                tracing::warn!(error = %e, "Feedback: push failed");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Evolution: failed to parse analysis")
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Evolution: LLM call failed");
                }
            }
        });
    }
}

// ── Boot / lifecycle ───────────────────────────────────────

impl CarrierKernel {
    /// Boot the kernel with configuration from the given path.
    pub fn boot(config_path: Option<&Path>) -> KernelResult<Self> {
        let config = load_config(config_path);
        Self::boot_with_config(config)
    }

    /// Boot the kernel with an explicit configuration.
    pub fn boot_with_config(mut config: KernelConfig) -> KernelResult<Self> {
        use types::config::KernelMode;

        // Env var overrides — useful for Docker where config.toml is baked in.
        if let Ok(listen) = std::env::var("OPENCARRIER_LISTEN") {
            config.api_listen = listen;
        }

        // OPENCARRIER_API_KEY: env var sets the API authentication key when
        // config.toml doesn't already have one.  Config file takes precedence.
        if config.api_key.trim().is_empty() {
            if let Ok(key) = std::env::var("OPENCARRIER_API_KEY") {
                let key = key.trim().to_string();
                if !key.is_empty() {
                    info!("Using API key from OPENCARRIER_API_KEY environment variable");
                    config.api_key = key;
                }
            }
        }

        // Clamp configuration bounds to prevent zero-value or unbounded misconfigs
        config.clamp_bounds();

        match config.mode {
            KernelMode::Stable => {
                info!("Booting Carrier kernel in STABLE mode — conservative defaults enforced");
            }
            KernelMode::Dev => {
                warn!("Booting Carrier kernel in DEV mode — experimental features enabled");
            }
            KernelMode::Default => {
                info!("Booting Carrier kernel...");
            }
        }

        // Validate configuration and log warnings
        let warnings = config.validate();
        for w in &warnings {
            warn!("Config: {}", w);
        }

        // Ensure data directory exists
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|e| KernelError::BootFailed(format!("Failed to create data dir: {e}")))?;

        // Initialize memory substrate
        let db_path = config
            .memory
            .sqlite_path
            .clone()
            .unwrap_or_else(|| config.data_dir.join("opencarrier.db"));
        let memory = Arc::new(
            MemorySubstrate::open(&db_path, config.memory.decay_rate)
                .map_err(|e| KernelError::BootFailed(format!("Memory init failed: {e}")))?,
        );

        // ── Auto-migrate admin tenant from config.toml ──────────────
        // ── Load Brain (carrier's independent LLM brain) ──────────────
        // Brain is required — boot fails without a valid brain.json.
        let brain_path = config.home_dir.join(&config.brain.config);
        let brain = if brain_path.exists() {
            let json_str = std::fs::read_to_string(&brain_path).map_err(|e| {
                KernelError::BootFailed(format!("Cannot read {}: {e}", brain_path.display()))
            })?;
            let brain_config: types::brain::BrainConfig =
                serde_json::from_str(&json_str)
                    .map_err(|e| KernelError::BootFailed(format!("Invalid brain.json: {e}")))?;
            let brain = Brain::new(brain_config)
                .map_err(|e| KernelError::BootFailed(format!("Brain init failed: {e}")))?;
            info!("Brain loaded from {}", brain_path.display());
            brain
        } else {
            // No local brain.json — try fetching from Hub.
            info!("Brain config not found locally; attempting to fetch from Hub...");
            match Self::fetch_brain_from_hub(&config.hub, &brain_path) {
                Ok(brain_config) => {
                    let brain = Brain::new(brain_config)
                        .map_err(|e| KernelError::BootFailed(format!("Brain init failed: {e}")))?;
                    info!(
                        "Brain fetched from Hub and saved to {}",
                        brain_path.display()
                    );
                    brain
                }
                Err(e) => {
                    return Err(KernelError::BootFailed(format!(
                        "Brain config not found at {} and could not be fetched from Hub: {}. \
                         Please set {} or create brain.json manually.",
                        brain_path.display(),
                        e,
                        config.hub.api_key_env
                    )));
                }
            }
        };

        // Initialize metering engine (shares the same SQLite connection as the memory substrate)
        let metering = Arc::new(MeteringEngine::new(
            Arc::new(memory::usage::UsageStore::new(memory.usage_conn())),
            config.budget.clone(),
        ));

        let supervisor = Supervisor::new();
        let background = BackgroundExecutor::new(supervisor.subscribe());

        // Initialize WASM sandbox engine (shared across all WASM agents)
        let wasm_sandbox = WasmSandbox::new()
            .map_err(|e| KernelError::BootFailed(format!("WASM sandbox init failed: {e}")))?;

        // Initialize model catalog, detect provider auth, and apply URL overrides
        let mut model_catalog = runtime::model_catalog::ModelCatalog::new();
        model_catalog.detect_auth();
        if !config.provider_urls.is_empty() {
            model_catalog.apply_url_overrides(&config.provider_urls);
            info!(
                "applied {} provider URL override(s)",
                config.provider_urls.len()
            );
        }
        // Load user's custom models from ~/.carrier/custom_models.json
        let custom_models_path = config.home_dir.join("custom_models.json");
        model_catalog.load_custom_models(&custom_models_path);
        let total_count = model_catalog.list_models().len();
        let provider_count = model_catalog.list_providers().len();
        info!("Model catalog: {total_count} models, {provider_count} providers");

        // MCP server list: use config directly (no extension merging)
        let all_mcp_servers = config.mcp_servers.clone();

        // Initialize web tools (free search + SSRF-protected fetch + caching)
        let cache_ttl = std::time::Duration::from_secs(config.web.cache_ttl_minutes * 60);
        let web_cache = Arc::new(runtime::web_cache::WebCache::new(cache_ttl));
        let brain_arc: Arc<Brain> = Arc::new(brain);
        let web_ctx = runtime::web_search::WebToolsContext {
            search: runtime::web_search::WebSearchEngine::new(web_cache.clone()),
            fetch: runtime::web_fetch::WebFetchEngine::new(
                config.web.fetch.clone(),
                web_cache,
            ),
            brain: Some(brain_arc.clone() as Arc<dyn runtime::llm_driver::Brain>),
        };

        // Initialize media understanding engine
        let media_engine =
            runtime::media_understanding::MediaEngine::new(config.media.clone());

        // Initialize cron scheduler
        let cron_scheduler =
            crate::cron::CronScheduler::new(&config.home_dir, config.max_cron_jobs);
        match cron_scheduler.load() {
            Ok(count) => {
                if count > 0 {
                    info!("Loaded {count} cron job(s) from disk");
                }
            }
            Err(e) => {
                warn!("Failed to load cron jobs: {e}");
            }
        }

        // Initialize binding/broadcast from config
        let initial_bindings = config.bindings.clone();
        let initial_broadcast = config.broadcast.clone();

        let kernel = Self {
            config,
            registry: AgentRegistry::new(),
            memory: memory.clone(),
            audit_log: Arc::new(AuditLog::with_db(memory.usage_conn())),
            metering,
            cron_scheduler,
            brain: KernelBrain {
                brain: Arc::new(std::sync::RwLock::new(brain_arc)),
                brain_path: brain_path.clone(),
                model_catalog: std::sync::RwLock::new(model_catalog),
            },
            a2a: KernelA2a {
                a2a_task_store: runtime::a2a::A2aTaskStore::default(),
                a2a_external_agents: std::sync::Mutex::new(Vec::new()),
            },
            services: KernelServices {
                web_ctx,
                media_engine,
            },
            plugins: KernelPlugins {
                mcp_connections: dashmap::DashMap::new(),
                mcp_tools: std::sync::Mutex::new(Vec::new()),
                toolset_registry: std::sync::RwLock::new(std::collections::HashMap::new()),
                effective_mcp_servers: std::sync::RwLock::new(all_mcp_servers),
                plugin_tool_dispatcher: std::sync::Mutex::new(None),
                mcp_reconnect_failures: dashmap::DashMap::new(),
            },
            runtime: KernelRuntime {
                scheduler: AgentScheduler::new(),
                supervisor,
                background,
                running_tasks: dashmap::DashMap::new(),
                wasm_sandbox,
                agent_msg_locks: dashmap::DashMap::new(),
                watcher_handles: std::sync::Mutex::new(Vec::new()),
            },
            coordination: KernelCoordination {
                capabilities: CapabilityManager::new(),
                event_bus: EventBus::new(),
                bindings: std::sync::Mutex::new(initial_bindings),
                broadcast: initial_broadcast,
                hooks: runtime::hooks::HookRegistry::new(),
                process_manager: Arc::new(runtime::process_manager::ProcessManager::new(5)),
                booted_at: std::time::Instant::now(),
                self_handle: OnceLock::new(),
            },
        };

        // Restore persisted agents from SQLite
        match kernel.memory.load_all_agents() {
            Ok(agents) => {
                let count = agents.len();
                for entry in agents {
                    let agent_id = entry.id;
                    let name = entry.name.clone();

                    let mut entry = entry;

                    let ws = kernel.config.effective_workspaces_dir().join(&name);
                    entry.manifest.workspace = Some(ws.clone());

                    // Hot-reload agent.toml if it exists — picks up tool/capability changes
                    // made to the workspace without needing an explicit restart.
                    let toml_path = ws.join("agent.toml");
                    if toml_path.exists() {
                        if let Ok(toml_str) = std::fs::read_to_string(&toml_path) {
                            if let Ok(disk_manifest) = toml::from_str::<AgentManifest>(&toml_str) {
                                let mut disk_manifest = disk_manifest;
                                disk_manifest.workspace = Some(ws.clone());
                                if disk_manifest.exec_policy.is_none() {
                                    disk_manifest.exec_policy =
                                        Some(kernel.config.exec_policy.clone());
                                }
                                if disk_manifest.model.modality.is_empty() {
                                    disk_manifest.model.modality = "chat".to_string();
                                }
                                entry.manifest = disk_manifest;
                                tracing::info!(agent = %name, "Hot-reloaded manifest from agent.toml on boot");
                            }
                        }
                    }

                    // Re-grant capabilities
                    let caps = manifest_to_capabilities(&entry.manifest);
                    kernel.coordination.capabilities.grant(agent_id, caps);

                    // Re-register with scheduler
                    kernel
                        .runtime
                        .scheduler
                        .register(agent_id, entry.manifest.resources.clone());

                    // Re-register in the in-memory registry.
                    // Restore Running agents as-is; promote Created/Suspended → Running
                    // so agents resume after service restarts without manual intervention.
                    let mut restored_entry = entry;
                    if restored_entry.state == AgentState::Created
                        || restored_entry.state == AgentState::Suspended
                    {
                        restored_entry.state = AgentState::Running;
                    }

                    // Inherit kernel exec_policy for agents that lack one
                    if restored_entry.manifest.exec_policy.is_none() {
                        restored_entry.manifest.exec_policy =
                            Some(kernel.config.exec_policy.clone());
                    }

                    // Apply default modality to restored agents if empty.
                    {
                        if restored_entry.manifest.model.modality.is_empty() {
                            restored_entry.manifest.model.modality = "chat".to_string();
                        }
                    }

                    if let Err(e) = kernel.registry.register(restored_entry) {
                        tracing::warn!(agent = %name, "Failed to restore agent: {e}");
                    } else {
                        tracing::debug!(agent = %name, id = %agent_id, "Restored agent");
                    }
                }
                if count > 0 {
                    info!("Restored {count} agent(s) from persistent storage");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load persisted agents: {e}");
            }
        }

        // Boot validation complete

        info!("Carrier kernel booted successfully");
        Ok(kernel)
    }

    /// Spawn a new agent from a manifest, optionally linking to a parent agent.
    pub fn spawn_agent(&self, manifest: AgentManifest) -> KernelResult<AgentId> {
        self.spawn_agent_with_parent(manifest, None, None)
    }

    /// Spawn a new agent with an optional parent for lineage tracking.
    /// If fixed_id is provided, use it instead of generating a new UUID.
    /// If tenant_id is provided, the agent and its workspace are scoped to that tenant.
    pub fn spawn_agent_with_parent(
        &self,
        manifest: AgentManifest,
        parent: Option<AgentId>,
        fixed_id: Option<AgentId>,
    ) -> KernelResult<AgentId> {
        let agent_id = fixed_id.unwrap_or_default();
        let session_id = SessionId::new();
        let name = manifest.name.clone();

        // SECURITY: Validate agent name doesn't contain path traversal characters
        if name.contains('/') || name.contains('\\') || name.contains("..") || name.is_empty() {
            return Err(KernelError::Carrier(
                types::error::CarrierError::InvalidInput(format!(
                    "Invalid agent name {:?}: must not contain path separators or '..'",
                    name
                )),
            ));
        }

        info!(agent = %name, id = %agent_id, parent = ?parent, "Spawning agent");

        // Create session
        self.memory
            .create_session(agent_id)
            .map_err(KernelError::Carrier)?;

        // Inherit kernel exec_policy as fallback if agent manifest doesn't have one
        let mut manifest = manifest;
        if manifest.exec_policy.is_none() {
            manifest.exec_policy = Some(self.config.exec_policy.clone());
        }
        info!(agent = %name, id = %agent_id, exec_mode = ?manifest.exec_policy.as_ref().map(|p| &p.mode), "Agent exec_policy resolved");

        // Overlay kernel default_model onto agent if agent didn't explicitly choose.
        // Treat empty or "default" as "use the kernel's configured default_model".
        // This allows bundled agents to defer to the user's configured provider/model,
        // even if the agent manifest specifies an api_key_env (which is just a hint
        // about which env var to check, not a hard lock on provider/model).
        // Create workspace directory for the agent (name-based, so SOUL.md survives recreation)
        let workspace_dir = manifest
            .workspace
            .clone()
            .unwrap_or_else(|| self.config.effective_workspaces_dir().join(&name));
        ensure_workspace(&workspace_dir)?;
        if manifest.generate_identity_files {
            generate_identity_files(&workspace_dir, &manifest);
        }
        manifest.workspace = Some(workspace_dir);

        // Register capabilities
        let caps = manifest_to_capabilities(&manifest);
        self.coordination.capabilities.grant(agent_id, caps);

        // Register with scheduler
        self.runtime
            .scheduler
            .register(agent_id, manifest.resources.clone());

        // Create registry entry
        let tags = manifest.tags.clone();
        let entry = AgentEntry {
            id: agent_id,
            name: manifest.name.clone(),
            manifest,
            state: AgentState::Running,
            mode: AgentMode::default(),
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            parent,
            children: vec![],
            session_id,
            tags,
            identity: Default::default(),
            onboarding_completed: false,
            onboarding_completed_at: None,
        };
        self.registry
            .register(entry.clone())
            .map_err(KernelError::Carrier)?;

        // Update parent's children list
        if let Some(parent_id) = parent {
            self.registry.add_child(parent_id, agent_id);
        }

        // Persist agent to SQLite so it survives restarts
        self.memory
            .save_agent(&entry)
            .map_err(KernelError::Carrier)?;

        info!(agent = %name, id = %agent_id, "Agent spawned");

        // SECURITY: Record agent spawn in audit trail
        self.audit_log.record(
            agent_id.to_string(),
            runtime::audit::AuditAction::AgentSpawn,
            format!("name={name}, parent={parent:?}"),
            "ok",
        );

        Ok(agent_id)
    }

    /// Verify a signed manifest envelope (Ed25519 + SHA-256).
    ///
    /// Call this before `spawn_agent` when a `SignedManifest` JSON is provided
    /// alongside the TOML. Returns the verified manifest TOML string on success.
    pub fn verify_signed_manifest(&self, signed_json: &str) -> KernelResult<String> {
        let signed: types::manifest_signing::SignedManifest =
            serde_json::from_str(signed_json).map_err(|e| {
                KernelError::Carrier(types::error::CarrierError::Config(format!(
                    "Invalid signed manifest JSON: {e}"
                )))
            })?;
        signed.verify().map_err(|e| {
            KernelError::Carrier(types::error::CarrierError::Config(format!(
                "Manifest signature verification failed: {e}"
            )))
        })?;
        info!(signer = %signed.signer_id, hash = %signed.content_hash, "Signed manifest verified");
        Ok(signed.manifest)
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::manifest_to_capabilities;
    use types::capability::Capability;
    use std::collections::HashMap;

    #[test]
    fn test_manifest_to_capabilities() {
        let mut manifest = AgentManifest {
            name: "test".to_string(),
            display_name: String::new(),
            version: "0.1.0".to_string(),
            description: "test".to_string(),
            author: "test".to_string(),
            module: "test".to_string(),
            schedule: ScheduleMode::default(),
            model: ModelConfig::default(),
            resources: ResourceQuota::default(),
            priority: Priority::default(),
            capabilities: ManifestCapabilities::default(),
            profile: None,
            tools: HashMap::new(),
            skills: vec![],
            mcp_servers: vec![],
            auto_load_toolsets: vec![],
            metadata: HashMap::new(),
            tags: vec![],
            autonomous: None,
            workspace: None,
            generate_identity_files: true,
            clone_source: None,
            exec_policy: None,
            tool_allowlist: vec![],
            tool_blocklist: vec![],
            knowledge_files: vec![],
            plugins: vec![],
        };
        manifest.capabilities.tools = vec!["file_read".to_string(), "web_search".to_string()];
        let caps = manifest_to_capabilities(&manifest);
        assert!(caps.iter().any(|c| matches!(c, Capability::ToolInvoke(t) if t == "file_read")));
        assert!(caps.iter().any(|c| matches!(c, Capability::ToolInvoke(t) if t == "web_search")));
    }

    fn test_manifest(name: &str, description: &str, tags: Vec<String>) -> AgentManifest {
        AgentManifest {
            name: name.to_string(),
            display_name: String::new(),
            version: "0.1.0".to_string(),
            description: description.to_string(),
            author: "test".to_string(),
            module: "test".to_string(),
            schedule: ScheduleMode::default(),
            model: ModelConfig::default(),
            resources: ResourceQuota::default(),
            priority: Priority::default(),
            capabilities: ManifestCapabilities::default(),
            profile: None,
            tools: HashMap::new(),
            skills: vec![],
            mcp_servers: vec![],
            auto_load_toolsets: vec![],
            metadata: HashMap::new(),
            tags,
            autonomous: None,
            workspace: None,
            generate_identity_files: true,
            clone_source: None,
            exec_policy: None,
            tool_allowlist: vec![],
            tool_blocklist: vec![],
            knowledge_files: vec![],
            plugins: vec![],
        }
    }

    fn register_test_agent(registry: &AgentRegistry, name: &str, desc: &str, tags: Vec<String>) -> AgentId {
        use types::agent::{AgentEntry, AgentIdentity, AgentMode, AgentState, SessionId};
        let id = AgentId::new();
        let entry = AgentEntry {
            id,
            name: name.to_string(),
            manifest: test_manifest(name, desc, tags),
            state: AgentState::Running,
            mode: AgentMode::default(),
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            parent: None,
            children: vec![],
            session_id: SessionId::new(),
            tags: vec![],
            identity: AgentIdentity::default(),
            onboarding_completed: false,
            onboarding_completed_at: None,
        };
        registry.register(entry).unwrap();
        id
    }

    #[test]
    fn test_send_to_agent_by_name_resolution() {
        let registry = AgentRegistry::new();
        let id = register_test_agent(&registry, "alice", "Alice agent", vec!["helper".to_string()]);
        assert!(registry.get(id).is_some());
        let found = registry.find_by_name("alice");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, id);
    }

    #[test]
    fn test_find_agents_by_tag() {
        let registry = AgentRegistry::new();
        register_test_agent(&registry, "bob", "Bob agent", vec!["coding".to_string()]);
        register_test_agent(&registry, "carol", "Carol agent", vec!["writing".to_string()]);
        let all = registry.list();
        let coding: Vec<_> = all.iter().filter(|a| a.manifest.tags.contains(&"coding".to_string())).collect();
        assert_eq!(coding.len(), 1);
        assert_eq!(coding[0].name, "bob");
    }

    #[test]
    fn test_manifest_to_capabilities_with_profile() {
        let mut manifest = test_manifest("profiled", "test", vec![]);
        manifest.profile = Some(types::agent::ToolProfile::Coding);
        let caps = manifest_to_capabilities(&manifest);
        assert!(!caps.is_empty());
    }

    #[test]
    fn test_manifest_to_capabilities_profile_overridden_by_explicit_tools() {
        let mut manifest = test_manifest("override", "test", vec![]);
        manifest.profile = Some(types::agent::ToolProfile::Coding);
        manifest.capabilities.tools = vec!["file_read".to_string()];
        let caps = manifest_to_capabilities(&manifest);
        assert_eq!(caps.len(), 1);
        assert!(caps.iter().any(|c| matches!(c, Capability::ToolInvoke(t) if t == "file_read")));
    }
}
