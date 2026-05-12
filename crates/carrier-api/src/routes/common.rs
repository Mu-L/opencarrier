//! Shared helpers used by multiple route handlers.

use axum::http::StatusCode;
use axum::Json;
use carrier_types::agent::{AgentEntry, AgentId};

/// Parse a path-parameter agent ID, returning BAD_REQUEST on failure.
pub fn parse_agent_id(id: &str) -> Result<AgentId, (StatusCode, Json<serde_json::Value>)> {
    id.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid agent ID"})),
        )
    })
}

/// Look up an agent in the registry, returning NOT_FOUND if missing.
pub fn get_agent_or_404(
    registry: &carrier_kernel::registry::AgentRegistry,
    agent_id: &AgentId,
) -> Result<AgentEntry, (StatusCode, Json<serde_json::Value>)> {
    registry.get(*agent_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )
    })
}

/// Parse agent ID from path and look up the agent. Returns (AgentId, AgentEntry) or an error response.
pub fn parse_and_get_agent(
    id: &str,
    registry: &carrier_kernel::registry::AgentRegistry,
) -> Result<(AgentId, AgentEntry), (StatusCode, Json<serde_json::Value>)> {
    let agent_id = parse_agent_id(id)?;
    let entry = get_agent_or_404(registry, &agent_id)?;
    Ok((agent_id, entry))
}

/// Resolve an agent by UUID or name.
///
/// - UUID: look up by ID.
/// - Name: look up globally.
pub fn resolve_agent_id(
    id_or_name: &str,
    registry: &carrier_kernel::registry::AgentRegistry,
) -> Result<(AgentId, AgentEntry), (StatusCode, Json<serde_json::Value>)> {
    // Try UUID first
    if let Ok(id) = id_or_name.parse::<AgentId>() {
        let entry = get_agent_or_404(registry, &id)?;
        return Ok((id, entry));
    }
    // Name lookup
    let entry = registry.find_by_name(id_or_name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent not found: {id_or_name}")})),
        )
    })?;
    Ok((entry.id, entry))
}

/// Look up a clone by name and extract its workspace path.
/// Returns (AgentEntry, PathBuf) or an error response.
pub fn get_clone_workspace(
    name: &str,
    registry: &carrier_kernel::registry::AgentRegistry,
) -> Result<(AgentEntry, std::path::PathBuf), (StatusCode, Json<serde_json::Value>)> {
    let entry = registry.find_by_name(name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Clone '{name}' not found")})),
        )
    })?;
    let workspace = entry.manifest.workspace.clone().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Agent has no workspace"})),
        )
    })?;
    Ok((entry, workspace))
}

// ---------------------------------------------------------------------------
// Shared upload registry (used by files, messaging, and sessions modules)
// ---------------------------------------------------------------------------

use dashmap::DashMap;
use std::sync::LazyLock;

/// Metadata stored alongside uploaded files.
pub struct UploadMeta {
    pub content_type: String,
    pub created_at: std::time::Instant,
}

/// In-memory upload metadata registry.
pub static UPLOAD_REGISTRY: LazyLock<DashMap<String, UploadMeta>> = LazyLock::new(DashMap::new);

/// Remove uploads older than 30 minutes from the registry.
pub fn cleanup_expired_uploads() {
    let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(30 * 60);
    UPLOAD_REGISTRY.retain(|_, meta| meta.created_at > cutoff);
}

// ---------------------------------------------------------------------------
// Workspace identity file whitelist (used by agents and files modules)
// ---------------------------------------------------------------------------

/// Immutable identity files — can be created but never overwritten via the API.
pub const IMMUTABLE_IDENTITY_FILES: &[&str] = &["SOUL.md"];

/// Whitelisted workspace identity files that can be read/written via API.
pub const KNOWN_IDENTITY_FILES: &[&str] = &[
    "SOUL.md",
    "IDENTITY.md",
    "USER.md",
    "TOOLS.md",
    "MEMORY.md",
    "AGENTS.md",
    "BOOTSTRAP.md",
    "HEARTBEAT.md",
];
