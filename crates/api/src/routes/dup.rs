//! `dup` remote endpoints - opencarrier acts as a **stateless git-style
//! remote** for the local `dup` CLI (in opencarrier-clones). File-level sync,
//! no packed archive. The client holds the merge base; the server just serves
//! its current manifest/files and applies fast-forward pushes.
//!
//! - `GET  /api/clones/{name}/dup/manifest` -> current definition-layer manifest
//! - `GET  /api/clones/{name}/dup/file/{*path}` -> raw file bytes
//! - `POST /api/clones/{name}/dup/push` -> fast-forward apply (409 if remote evolved)

use crate::routes::common::get_clone_workspace;
use crate::routes::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use std::collections::BTreeMap;
use std::path::{Component, PathBuf};
use std::path::Path as StdPath;
use std::sync::Arc;

/// Runtime top-level entries push/file must never touch (mirror manifest::SKIP).
/// `api_tools.toml` is deployment-specific API tool config (not shareable via
/// dup/DupHub) - managed via `api_tool_register`, like `bind_agent`.
const SKIP: &[&str] = &[
    "agent.toml",
    "AGENT.json",
    "admins.json",
    "api_tools.toml",
    "output",
    "sessions",
    "history",
    "logs",
    "users",
    "data",
    "senders",
    ".lifecycle",
    ".dup",
];

pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route(
            "/api/clones/{name}/dup/manifest",
            routing::get(get_dup_manifest),
        )
        .route(
            "/api/clones/{name}/dup/file/{*path}",
            routing::get(get_dup_file),
        )
        .route("/api/clones/{name}/dup/push", routing::post(push_dup))
}

/// Build a JSON error response.
fn json_err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({"error": msg.into()}))).into_response()
}

fn server_err(msg: impl Into<String>) -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// GET /api/clones/{name}/dup/manifest -> `{hash, files:{path:sha256}}`.
pub async fn get_dup_manifest(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let (_entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    match clone::build_manifest(&workspace) {
        Ok(m) => Json(m).into_response(),
        Err(e) => server_err(format!("build manifest: {e}")),
    }
}

/// GET /api/clones/{name}/dup/file/{*path} -> raw file bytes (definition layer only).
pub async fn get_dup_file(
    State(state): State<Arc<AppState>>,
    Path((name, path)): Path<(String, String)>,
) -> Response {
    let (_entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    let file_path = match safe_path(&workspace, &path) {
        Ok(p) => p,
        Err((code, msg)) => return json_err(code, msg),
    };
    match std::fs::read(&file_path) {
        Ok(bytes) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream",
            )],
            Bytes::from(bytes),
        )
            .into_response(),
        Err(e) => json_err(StatusCode::NOT_FOUND, format!("read: {e}")),
    }
}

#[derive(serde::Deserialize)]
pub struct PushRequest {
    /// Manifest hash the client pulled last (= expected current server state).
    base_hash: String,
    /// path -> base64-encoded file content to apply.
    files: BTreeMap<String, String>,
    /// paths to delete.
    #[serde(default)]
    deletes: Vec<String>,
}

/// POST /api/clones/{name}/dup/push - fast-forward file apply.
///
/// Returns 409 Conflict if the server's current manifest hash differs from
/// `base_hash` (remote evolved since the client pulled -> client must pull first).
pub async fn push_dup(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<PushRequest>,
) -> Response {
    let (_entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };

    // Fast-forward check.
    let current = match clone::build_manifest(&workspace) {
        Ok(m) => m,
        Err(e) => return server_err(format!("build manifest: {e}")),
    };
    if current.hash != req.base_hash {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "remote evolved, pull first",
                "remote_hash": current.hash,
            })),
        )
            .into_response();
    }

    // Apply file writes.
    for (rel, b64) in &req.files {
        let file_path = match safe_path(&workspace, rel) {
            Ok(p) => p,
            Err((code, msg)) => return json_err(code, msg),
        };
        let content = match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
            Ok(b) => b,
            Err(e) => return json_err(StatusCode::BAD_REQUEST, format!("decode {rel}: {e}")),
        };
        if let Some(parent) = file_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return server_err(format!("mkdir {rel}: {e}"));
            }
        }
        if let Err(e) = atomic_write(&file_path, &content) {
            return server_err(format!("write {rel}: {e}"));
        }
    }

    // Apply deletes.
    for rel in &req.deletes {
        let file_path = match safe_path(&workspace, rel) {
            Ok(p) => p,
            Err((code, msg)) => return json_err(code, msg),
        };
        if file_path.exists() {
            if let Err(e) = std::fs::remove_file(&file_path) {
                return server_err(format!("delete {rel}: {e}"));
            }
        }
    }

    match clone::build_manifest(&workspace) {
        Ok(m) => Json(serde_json::json!({"status": "applied", "manifest": m})).into_response(),
        Err(e) => server_err(format!("rebuild manifest: {e}")),
    }
}

/// Resolve a relative path inside `workspace`, enforcing definition-layer +
/// traversal safety. Returns the absolute path or a `(status, message)` error.
fn safe_path(workspace: &StdPath, rel: &str) -> Result<PathBuf, (StatusCode, String)> {
    let p = StdPath::new(rel);
    if p
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
    {
        return Err((StatusCode::FORBIDDEN, "Path traversal denied".into()));
    }
    let top = rel.split('/').next().unwrap_or(rel);
    if SKIP.contains(&top) || clone::is_test_dir(top) || clone::is_bak(top) {
        return Err((StatusCode::FORBIDDEN, "Path not in definition layer".into()));
    }
    let ws_canonical = match workspace.canonicalize() {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::INTERNAL_SERVER_ERROR, "Workspace path error".into())),
    };
    let file_path = workspace.join(rel);
    // For new files, validate via the parent dir; for existing, via the file itself.
    let check = if file_path.exists() {
        file_path
            .canonicalize()
            .unwrap_or_else(|_| file_path.clone())
    } else {
        file_path
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(file_path.file_name().unwrap_or_default()))
            .unwrap_or_else(|| file_path.clone())
    };
    if !check.starts_with(&ws_canonical) {
        return Err((StatusCode::FORBIDDEN, "Path traversal denied".into()));
    }
    Ok(file_path)
}

/// Atomic write: write to a sibling `.duptmp` then rename.
fn atomic_write(file_path: &StdPath, content: &[u8]) -> std::io::Result<()> {
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = file_path.with_file_name(format!(".{filename}.duptmp"));
    std::fs::write(&tmp, content)?;
    if let Err(e) = std::fs::rename(&tmp, file_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}
