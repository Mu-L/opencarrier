//! Shared helpers used by multiple route handlers.

use axum::http::StatusCode;
use axum::Json;
use types::agent::{AgentEntry, AgentId, AgentIdentity};
use types::error::CarrierError;

/// Parse an agent ID or name from a path parameter, returning the resolved AgentId.
/// Accepts both UUID strings and agent names.
///
/// Delegates to `resolve_agent_id` (the canonical resolver) and discards the
/// entry — the error mapping lives in exactly one place.
pub fn resolve_agent_id_from_path(
    id: &str,
    registry: &kernel::registry::AgentRegistry,
) -> Result<AgentId, (StatusCode, Json<serde_json::Value>)> {
    resolve_agent_id(id, registry).map(|(aid, _)| aid)
}

/// Look up an agent in the registry, returning NOT_FOUND if missing.
pub fn get_agent_or_404(
    registry: &kernel::registry::AgentRegistry,
    agent_id: &AgentId,
) -> Result<AgentEntry, (StatusCode, Json<serde_json::Value>)> {
    registry.get(*agent_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )
    })
}

/// Resolve an agent by UUID or name — the single canonical resolver used by
/// every route handler (previously three wrappers with duplicated error
/// mapping: resolve_agent_id, parse_and_get_agent, resolve_agent_id_from_path).
///
/// Delegates to `AgentRegistry::resolve()` — the single source of truth.
pub fn resolve_agent_id(
    id_or_name: &str,
    registry: &kernel::registry::AgentRegistry,
) -> Result<(AgentId, AgentEntry), (StatusCode, Json<serde_json::Value>)> {
    registry.resolve(id_or_name).map_err(|e| match e {
        CarrierError::AgentNotFound(name) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent not found: {name}")})),
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    })
}

/// Resolve an agent UUID or name to just the agent name string.
///
/// Convenience wrapper for call sites that only need the name.
pub fn resolve_to_name(
    id_or_name: &str,
    registry: &kernel::registry::AgentRegistry,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let (_, entry) = resolve_agent_id(id_or_name, registry)?;
    Ok(entry.name.clone())
}

/// Look up a clone by name and extract its workspace path.
/// Returns (AgentEntry, PathBuf) or an error response.
pub fn get_clone_workspace(
    name: &str,
    registry: &kernel::registry::AgentRegistry,
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
// Shared validation limits + identity helpers (agents.rs PATCH endpoints)
// ---------------------------------------------------------------------------

/// Length limits for agent config fields — PATCH /api/agents/{id} and
/// PATCH /api/agents/{id}/config enforce the SAME rules (patch_agent used to
/// skip validation entirely while its /config twin enforced these).
pub const MAX_NAME_LEN: usize = 256;
pub const MAX_DESC_LEN: usize = 4096;
pub const MAX_PROMPT_LEN: usize = 65_536;

/// Validate identity field formats (color hex, avatar URL scheme).
/// Previously duplicated verbatim in update_agent_identity and
/// patch_agent_config — a rule change in one left the other stale.
pub fn validate_identity_formats(
    color: Option<&str>,
    avatar_url: Option<&str>,
) -> Result<(), &'static str> {
    if let Some(color) = color {
        if !color.is_empty() && !color.starts_with('#') {
            return Err("Color must be a hex code starting with '#'");
        }
    }
    if let Some(url) = avatar_url {
        if !url.is_empty()
            && !url.starts_with("http://")
            && !url.starts_with("https://")
            && !url.starts_with("data:")
        {
            return Err("Avatar URL must be http/https or data URI");
        }
    }
    Ok(())
}

/// Merge an identity update over the agent's current identity (None = keep
/// current). Shared by PATCH /{id}/identity and PATCH /{id}/config — the two
/// endpoints used to have OPPOSITE semantics (identity REPLACED, wiping unset
/// fields to None on a partial update; config merged).
pub fn merge_identity(
    registry: &kernel::registry::AgentRegistry,
    agent_id: AgentId,
    update: AgentIdentity,
) -> AgentIdentity {
    let current = registry.get(agent_id).map(|e| e.identity).unwrap_or_default();
    AgentIdentity {
        emoji: update.emoji.or(current.emoji),
        avatar_url: update.avatar_url.or(current.avatar_url),
        color: update.color.or(current.color),
        archetype: update.archetype.or(current.archetype),
        vibe: update.vibe.or(current.vibe),
        greeting_style: update.greeting_style.or(current.greeting_style),
    }
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
