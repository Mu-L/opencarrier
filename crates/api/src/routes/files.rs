//! File upload and agent file management endpoints.

use crate::routes::common::*;
use crate::routes::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;

/// Request body for writing a workspace identity file.
#[derive(serde::Deserialize)]
pub struct SetAgentFileRequest {
    pub content: String,
}

/// GET /api/agents/{id}/files — List workspace identity files.
pub async fn list_agent_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (_agent_id, entry) = match parse_and_get_agent(&id, &state.kernel.registry) {
        Ok(r) => r,
        Err((status, _)) => {
            return (
                status,
                Json(serde_json::json!({"error": "Agent not found"})),
            );
        }
    };

    let workspace = match entry.manifest.workspace {
        Some(ref ws) => ws.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Agent has no workspace"})),
            );
        }
    };

    let mut files = Vec::new();
    for &name in KNOWN_IDENTITY_FILES {
        let path = workspace.join(name);
        let (exists, size_bytes) = if path.exists() {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            (true, size)
        } else {
            (false, 0u64)
        };
        files.push(serde_json::json!({
            "name": name,
            "exists": exists,
            "size_bytes": size_bytes,
        }));
    }

    (StatusCode::OK, Json(serde_json::json!({ "files": files })))
}
/// GET /api/agents/{id}/files/{filename} — Read a workspace identity file.
pub async fn get_agent_file(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
) -> impl IntoResponse {
    let agent_id = match resolve_agent_id_from_path(&id, &state.kernel.registry) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // Validate filename whitelist
    if !KNOWN_IDENTITY_FILES.contains(&filename.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "File not in whitelist"})),
        );
    }

    let entry = match get_agent_or_404(&state.kernel.registry, &agent_id) {
        Ok(e) => e,
        Err(r) => return r,
    };

    let workspace = match entry.manifest.workspace {
        Some(ref ws) => ws.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Agent has no workspace"})),
            );
        }
    };

    // Security: canonicalize and verify stays inside workspace
    let file_path = workspace.join(&filename);
    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "File not found"})),
            );
        }
    };
    let ws_canonical = match workspace.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Workspace path error"})),
            );
        }
    };
    if !canonical.starts_with(&ws_canonical) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Path traversal denied"})),
        );
    }

    let content = match std::fs::read_to_string(&canonical) {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "File not found"})),
            );
        }
    };

    let size_bytes = content.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "name": filename,
            "content": content,
            "size_bytes": size_bytes,
        })),
    )
}
/// PUT /api/agents/{id}/files/{filename} — Write a workspace identity file.
///
/// Immutable files (SOUL.md) cannot be overwritten once created.
pub async fn set_agent_file(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
    Json(req): Json<SetAgentFileRequest>,
) -> impl IntoResponse {
    let agent_id = match resolve_agent_id_from_path(&id, &state.kernel.registry) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // Validate filename whitelist
    if !KNOWN_IDENTITY_FILES.contains(&filename.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "File not in whitelist"})),
        );
    }

    // Immutable files: cannot be overwritten once created
    if IMMUTABLE_IDENTITY_FILES.contains(&filename.as_str()) {
        let entry = match get_agent_or_404(&state.kernel.registry, &agent_id) {
            Ok(e) => e,
            Err(r) => return r,
        };
        if let Some(ref workspace) = entry.manifest.workspace {
            let file_path = workspace.join(&*filename);
            if file_path.exists() {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": format!("{} is immutable — it cannot be overwritten after creation. \
                        This file defines the clone's identity and must not be tampered with.", filename)
                    })),
                );
            }
        }
    }

    // Max 32KB content
    const MAX_FILE_SIZE: usize = 32_768;
    if req.content.len() > MAX_FILE_SIZE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "File content too large (max 32KB)"})),
        );
    }

    let entry = match get_agent_or_404(&state.kernel.registry, &agent_id) {
        Ok(e) => e,
        Err(r) => return r,
    };

    let workspace = match entry.manifest.workspace {
        Some(ref ws) => ws.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Agent has no workspace"})),
            );
        }
    };

    // Security: verify workspace path and target stays inside it
    let ws_canonical = match workspace.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Workspace path error"})),
            );
        }
    };

    let file_path = workspace.join(&filename);
    // For new files, check the parent directory instead
    let check_path = if file_path.exists() {
        file_path
            .canonicalize()
            .unwrap_or_else(|_| file_path.clone())
    } else {
        // Parent must be inside workspace
        file_path
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(&filename))
            .unwrap_or_else(|| file_path.clone())
    };
    if !check_path.starts_with(&ws_canonical) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Path traversal denied"})),
        );
    }

    // Atomic write: write to .tmp, then rename
    let tmp_path = workspace.join(format!(".{filename}.tmp"));
    if let Err(e) = std::fs::write(&tmp_path, &req.content) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Write failed: {e}")})),
        );
    }
    if let Err(e) = std::fs::rename(&tmp_path, &file_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Rename failed: {e}")})),
        );
    }

    let size_bytes = req.content.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "name": filename,
            "size_bytes": size_bytes,
        })),
    )
}
// ---------------------------------------------------------------------------
// File Upload endpoints
// ---------------------------------------------------------------------------

/// Response body for file uploads.
#[derive(serde::Serialize)]
struct UploadResponse {
    file_id: String,
    filename: String,
    content_type: String,
    size: usize,
    /// Transcription text for audio uploads (populated via Whisper STT).
    #[serde(skip_serializing_if = "Option::is_none")]
    transcription: Option<String>,
}
/// Maximum upload size: 10 MB.
const MAX_UPLOAD_SIZE: usize = 10 * 1024 * 1024;
/// Allowed content type prefixes for upload.
const ALLOWED_CONTENT_TYPES: &[&str] = &["image/", "text/", "application/pdf", "audio/"];

fn is_allowed_content_type(ct: &str) -> bool {
    ALLOWED_CONTENT_TYPES
        .iter()
        .any(|prefix| ct.starts_with(prefix))
}
/// POST /api/agents/{id}/upload — Upload a file attachment.
///
/// Accepts raw body bytes. The client must set:
/// - `Content-Type` header (e.g., `image/png`, `text/plain`, `application/pdf`)
/// - `X-Filename` header (original filename)
pub async fn upload_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    cleanup_expired_uploads();

    // Validate agent ID format
    let agent_id = match resolve_agent_id_from_path(&id, &state.kernel.registry) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // Extract content type
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    if !is_allowed_content_type(&content_type) {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "Unsupported content type. Allowed: image/*, text/*, audio/*, application/pdf"}),
            ),
        );
    }

    // Extract filename from header
    let raw_filename = headers
        .get("X-Filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("upload");
    let filename = types::config::sanitize_path_component(raw_filename).to_string();

    // Extract sender_id from header (used to place file in per-user input dir)
    let sender_id = headers
        .get("X-Sender-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let owner_id = headers
        .get("X-Owner-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Validate size
    if body.len() > MAX_UPLOAD_SIZE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(
                serde_json::json!({"error": format!("File too large (max {} MB)", MAX_UPLOAD_SIZE / (1024 * 1024))}),
            ),
        );
    }

    if body.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Empty file body"})),
        );
    }

    // Generate file ID
    let file_id = uuid::Uuid::new_v4().to_string();

    // Determine save location: prefer senders/{sender_id}/{agent_name}/input/
    // Fall back to temp dir if no workspace or no sender_id
    let (file_path, upload_dir) = if let Some(ref sid) = sender_id {
        if let Some(entry) = state.kernel.registry.get(agent_id) {
            if let Some(ref _ws) = entry.manifest.workspace {
                let agent_name = &entry.manifest.name;
                let user_input_dir = types::config::sender_data_dir(
                    &state.kernel.config.home_dir, owner_id.as_deref().unwrap_or(sid), agent_name, Some(sid),
                ).join("input");
                if let Err(e) = std::fs::create_dir_all(&user_input_dir) {
                    tracing::warn!("Failed to create user input dir: {e}");
                } else {
                    let dest = user_input_dir.join(&filename);
                    if let Err(e) = std::fs::write(&dest, &body) {
                        tracing::warn!("Failed to write upload to workspace: {e}");
                    } else {
                        tracing::info!(agent = %id, sender = %sid, file = %filename, "File uploaded to user input dir");
                        // Also save to temp for image resolution (attachments flow)
                        let tmp_dir = std::env::temp_dir().join("carrier_uploads");
                        let _ = std::fs::create_dir_all(&tmp_dir);
                        let tmp_path = tmp_dir.join(&file_id);
                        let _ = std::fs::write(&tmp_path, &body);
                        let size = body.len();
                        UPLOAD_REGISTRY.insert(
                            file_id.clone(),
                            UploadMeta {
                                content_type: content_type.clone(),
                                created_at: std::time::Instant::now(),
                            },
                        );

                        // Auto-transcribe audio uploads using the media engine
                        let transcription = if content_type.starts_with("audio/") {
                            let attachment = types::media::MediaAttachment {
                                media_type: types::media::MediaType::Audio,
                                mime_type: content_type.clone(),
                                source: types::media::MediaSource::FilePath {
                                    path: dest.to_string_lossy().to_string(),
                                },
                                size_bytes: size as u64,
                            };
                            match state
                                .kernel
                                .services
                                .media_engine
                                .transcribe_audio(&attachment)
                                .await
                            {
                                Ok(result) => {
                                    tracing::info!(chars = result.description.len(), provider = %result.provider, "Audio transcribed");
                                    Some(result.description)
                                }
                                Err(e) => {
                                    tracing::warn!("Audio transcription failed: {e}");
                                    None
                                }
                            }
                        } else {
                            None
                        };

                        return (
                            StatusCode::CREATED,
                            Json(serde_json::json!(UploadResponse {
                                file_id,
                                filename,
                                content_type,
                                size,
                                transcription,
                            })),
                        );
                    }
                }
            }
        }
        // Fallback: temp dir
        let upload_dir = std::env::temp_dir().join("carrier_uploads");
        (upload_dir.join(&file_id), upload_dir)
    } else {
        // No sender_id — use temp dir (legacy behavior)
        let upload_dir = std::env::temp_dir().join("carrier_uploads");
        (upload_dir.join(&file_id), upload_dir)
    };

    if let Err(e) = std::fs::create_dir_all(&upload_dir) {
        tracing::warn!("Failed to create upload dir: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to create upload directory"})),
        );
    }

    if let Err(e) = std::fs::write(&file_path, &body) {
        tracing::warn!("Failed to write upload: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to save file"})),
        );
    }

    let size = body.len();
    UPLOAD_REGISTRY.insert(
        file_id.clone(),
        UploadMeta {
            content_type: content_type.clone(),
            created_at: std::time::Instant::now(),
        },
    );

    // Auto-transcribe audio uploads using the media engine
    let transcription = if content_type.starts_with("audio/") {
        let attachment = types::media::MediaAttachment {
            media_type: types::media::MediaType::Audio,
            mime_type: content_type.clone(),
            source: types::media::MediaSource::FilePath {
                path: file_path.to_string_lossy().to_string(),
            },
            size_bytes: size as u64,
        };
        match state
            .kernel
            .services
            .media_engine
            .transcribe_audio(&attachment)
            .await
        {
            Ok(result) => {
                tracing::info!(chars = result.description.len(), provider = %result.provider, "Audio transcribed");
                Some(result.description)
            }
            Err(e) => {
                tracing::warn!("Audio transcription failed: {e}");
                None
            }
        }
    } else {
        None
    };

    (
        StatusCode::CREATED,
        Json(serde_json::json!(UploadResponse {
            file_id,
            filename,
            content_type,
            size,
            transcription,
        })),
    )
}
/// GET /api/uploads/{file_id} — Serve an uploaded file.
pub async fn serve_upload(Path(file_id): Path<String>) -> impl IntoResponse {
    cleanup_expired_uploads();

    // Validate file_id is a UUID to prevent path traversal
    if uuid::Uuid::parse_str(&file_id).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            b"{\"error\":\"Invalid file ID\"}".to_vec(),
        );
    }

    // File exists in upload registry — no tenant check needed (tenant layer removed)

    let file_path = std::env::temp_dir().join("carrier_uploads").join(&file_id);

    // Look up metadata from registry; fall back to disk probe for generated images
    // (image_generate saves files without registering in UPLOAD_REGISTRY).
    let content_type = match UPLOAD_REGISTRY.get(&file_id) {
        Some(m) => m.content_type.clone(),
        None => {
            // Infer content type from file magic bytes
            if !file_path.exists() {
                return (
                    StatusCode::NOT_FOUND,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/json".to_string(),
                    )],
                    b"{\"error\":\"File not found\"}".to_vec(),
                );
            }
            "image/png".to_string()
        }
    };

    match std::fs::read(&file_path) {
        Ok(data) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, content_type)],
            data,
        ),
        Err(_) => (
            StatusCode::NOT_FOUND,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            b"{\"error\":\"File not found on disk\"}".to_vec(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Agent Output File endpoints
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct OutputQuery {
    pub sender_id: String,
    pub owner_id: Option<String>,
}

/// GET /api/agents/{id}/output — List output files for a specific user.
///
/// Requires `?sender_id=xxx` query param. No fallback — sender_id is mandatory.
pub async fn list_output_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<OutputQuery>,
) -> impl IntoResponse {
    let (_agent_id, entry) = match parse_and_get_agent(&id, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let agent_name = &entry.manifest.name;
    let output_dir = types::config::sender_data_dir(
        &state.kernel.config.home_dir, params.owner_id.as_deref().unwrap_or(&params.sender_id), agent_name, Some(&params.sender_id),
    ).join("output");

    if !output_dir.exists() {
        return (StatusCode::OK, Json(serde_json::json!({"files": []})));
    }

    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&output_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let modified = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            files.push(serde_json::json!({
                "name": name,
                "size_bytes": size,
                "modified_at": modified,
            }));
        }
    }

    (StatusCode::OK, Json(serde_json::json!({"files": files})))
}

/// GET /api/agents/{id}/output/{*path} — Download a single output file.
///
/// Requires `?sender_id=xxx` query param. No fallback.
pub async fn serve_output_file(
    State(state): State<Arc<AppState>>,
    _extensions: axum::http::Extensions,
    Path((id, file_path)): Path<(String, String)>,
    Query(params): Query<OutputQuery>,
) -> axum::response::Response {
    let err = |status: StatusCode, msg: &str| -> axum::response::Response {
        (
            status,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            format!("{{\"error\":\"{}\"}}", msg).into_bytes(),
        )
            .into_response()
    };

    // Public download endpoint — skip tenant check. Path-based auth via sender_id
    // is sufficient: files are scoped to senders/{sender_id}/{agent_name}/output/.
    let (_agent_id, entry) = match parse_and_get_agent(&id, &state.kernel.registry) {
        Ok(pair) => pair,
        Err((status, _)) => {
            let msg = if status == StatusCode::BAD_REQUEST {
                "Invalid agent ID"
            } else {
                "Agent not found"
            };
            return err(status, msg);
        }
    };

    // Build the safe base: senders/{owner_id}/{agent_name}/.../output/
    let agent_name = &entry.manifest.name;
    let safe_base = types::config::sender_data_dir(
        &state.kernel.config.home_dir, params.owner_id.as_deref().unwrap_or(&params.sender_id), agent_name, Some(&params.sender_id),
    ).join("output");

    // Reject any ".." in file_path
    if file_path.contains("..") {
        return err(StatusCode::FORBIDDEN, "Path traversal denied");
    }

    let target = safe_base.join(&file_path);

    // Canonicalize and verify target stays inside safe_base
    let base_canonical = match safe_base.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "Output directory not found"),
    };
    let target_canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "File not found"),
    };

    if !target_canonical.starts_with(&base_canonical) {
        return err(StatusCode::FORBIDDEN, "Path traversal denied");
    }

    let data = match std::fs::read(&target_canonical) {
        Ok(d) => d,
        Err(_) => return err(StatusCode::NOT_FOUND, "File not found"),
    };

    // Extract filename for Content-Disposition
    let filename = file_path.rsplit('/').next().unwrap_or(&file_path);

    // Force download — never execute/render in browser
    let resp = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .header("x-content-type-options", "nosniff")
        .body(data.into())
        .unwrap();
    resp
}

// ---------------------------------------------------------------------------
// File Explorer endpoints — directory tree + browser-friendly file viewing
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct TreeQuery {
    pub sender_id: String,
    pub owner_id: Option<String>,
    pub subdir: Option<String>,
    pub path: Option<String>,
}

/// GET /api/files/tree/{agent} — List sender files and directories.
///
/// Supports drilling into subdirectories:
///   /api/files/tree/agent-name?sender_id=xxx
///   /api/files/tree/agent-name?sender_id=xxx&subdir=output&path=参考素材
pub async fn list_files_tree(
    State(state): State<Arc<AppState>>,
    Path(agent): Path<String>,
    Query(params): Query<TreeQuery>,
) -> impl IntoResponse {
    let err = |status: StatusCode, msg: &str| -> (StatusCode, Json<serde_json::Value>) {
        (status, Json(serde_json::json!({"error": msg})))
    };

    let (_agent_id, entry) = match parse_and_get_agent(&agent, &state.kernel.registry) {
        Ok(r) => r,
        Err((status, _)) => return err(status, "Agent not found"),
    };

    let agent_name = &entry.manifest.name;
    let subdir = params.subdir.as_deref().unwrap_or("output");
    let base = types::config::sender_data_dir(
        &state.kernel.config.home_dir,
        params.owner_id.as_deref().unwrap_or(&params.sender_id),
        agent_name,
        Some(&params.sender_id),
    )
    .join(subdir);

    let target = match &params.path {
        Some(p) if !p.is_empty() => {
            if p.contains("..") {
                return err(StatusCode::FORBIDDEN, "Path traversal denied");
            }
            base.join(p)
        }
        _ => base.clone(),
    };

    // Canonicalize and security check
    let base_canonical = match base.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "Directory not found"),
    };
    let target_canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "Directory not found"),
    };
    if !target_canonical.starts_with(&base_canonical) {
        return err(StatusCode::FORBIDDEN, "Path traversal denied");
    }

    let mut items = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&target_canonical) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = path.is_dir();
            let meta = std::fs::metadata(&path).ok();
            items.push(serde_json::json!({
                "name": name,
                "type": if is_dir { "dir" } else { "file" },
                "size_bytes": if !is_dir { meta.as_ref().map(|m| m.len()).unwrap_or(0) } else { 0 },
                "modified_at": meta.as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            }));
        }
    }

    // Sort: directories first, then files, alphabetically
    items.sort_by(|a, b| {
        let a_dir = a["type"].as_str() == Some("dir");
        let b_dir = b["type"].as_str() == Some("dir");
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a["name"].as_str().cmp(&b["name"].as_str()),
        }
    });

    (StatusCode::OK, Json(serde_json::json!({ "items": items, "base_path": subdir  })))
}

#[derive(serde::Deserialize)]
pub struct ViewQuery {
    pub sender_id: String,
    pub owner_id: Option<String>,
    pub render: Option<String>,
}

/// Map a file extension to a MIME type string.
fn mime_for_path(path: &str) -> &'static str {
    if path.ends_with(".md") {
        return "text/markdown; charset=utf-8";
    }
    if path.ends_with(".html") || path.ends_with(".htm") {
        return "text/html; charset=utf-8";
    }
    if path.ends_with(".json") {
        return "application/json; charset=utf-8";
    }
    if path.ends_with(".txt") || path.ends_with(".log") {
        return "text/plain; charset=utf-8";
    }
    if path.ends_with(".csv") {
        return "text/csv; charset=utf-8";
    }
    if path.ends_with(".png") {
        return "image/png";
    }
    if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        return "image/jpeg";
    }
    if path.ends_with(".gif") {
        return "image/gif";
    }
    if path.ends_with(".svg") {
        return "image/svg+xml";
    }
    if path.ends_with(".webp") {
        return "image/webp";
    }
    if path.ends_with(".pdf") {
        return "application/pdf";
    }
    "application/octet-stream"
}

/// GET /api/files/view/{agent}/{*path}?sender_id=xxx — Serve file for browser viewing.
///
/// Unlike serve_output_file, this endpoint:
/// - Sets Content-Type based on file extension
/// - Uses Content-Disposition: inline (not attachment)
/// - Supports ?render=markdown to render .md files as HTML via pulldown-cmark
///
/// Direct file links can be shared (e.g., from WeChat) since this is a GET-only public endpoint.
pub async fn view_file(
    State(state): State<Arc<AppState>>,
    Path((agent, file_path)): Path<(String, String)>,
    Query(params): Query<ViewQuery>,
) -> axum::response::Response {
    let err = |status: StatusCode, msg: &str| -> axum::response::Response {
        (
            status,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            format!("{{\"error\":\"{}\"}}", msg).into_bytes(),
        )
            .into_response()
    };

    let (_agent_id, entry) = match parse_and_get_agent(&agent, &state.kernel.registry) {
        Ok(pair) => pair,
        Err((status, _)) => return err(status, "Agent not found"),
    };

    let agent_name = &entry.manifest.name;
    let safe_base = types::config::sender_data_dir(
        &state.kernel.config.home_dir,
        params.owner_id.as_deref().unwrap_or(&params.sender_id),
        agent_name,
        Some(&params.sender_id),
    );

    // Path traversal prevention
    if file_path.contains("..") {
        return err(StatusCode::FORBIDDEN, "Path traversal denied");
    }

    let target = safe_base.join(&file_path);

    let base_canonical = match safe_base.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "Base directory not found"),
    };
    let target_canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => return err(StatusCode::NOT_FOUND, "File not found"),
    };
    if !target_canonical.starts_with(&base_canonical) {
        return err(StatusCode::FORBIDDEN, "Path traversal denied");
    }

    let data = match std::fs::read(&target_canonical) {
        Ok(d) => d,
        Err(_) => return err(StatusCode::NOT_FOUND, "File not found"),
    };

    let content_type = mime_for_path(&file_path);

    // Handle markdown rendering
    if params.render.as_deref() == Some("markdown") && file_path.ends_with(".md") {
        let text = String::from_utf8_lossy(&data);
        let mut html = String::new();
        let parser = pulldown_cmark::Parser::new(&text);
        pulldown_cmark::html::push_html(&mut html, parser);
        let styled = format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <style>body {{ background:#1a1a2e; color:#e0e0e0; font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif; \
             max-width:800px; margin:0 auto; padding:20px; line-height:1.6; }} \
             h1,h2,h3,h4 {{ color:#ffffff; }} \
             a {{ color:#64b5f6; }} \
             code {{ background:#2d2d44; padding:2px 6px; border-radius:3px; }} \
             pre {{ background:#2d2d44; padding:16px; border-radius:8px; overflow-x:auto; }} \
             img {{ max-width:100%; border-radius:4px; }} \
             blockquote {{ border-left:3px solid #64b5f6; margin:0; padding-left:16px; color:#aaa; }}</style></head>\
             <body>{}</body></html>",
            html
        );
        return axum::response::Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(styled.into_bytes().into())
            .unwrap();
    }

    // Serve with inline disposition for browser viewing
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header("content-disposition", "inline")
        .header("x-content-type-options", "nosniff")
        .body(data.into())
        .unwrap()
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/agents/{id}/files", routing::get(list_agent_files))
        .route(
            "/api/agents/{id}/files/{filename}",
            routing::put(set_agent_file).get(get_agent_file),
        )
        .route("/api/agents/{id}/upload", routing::post(upload_file))
        .route("/api/uploads/{file_id}", routing::get(serve_upload))
        .route("/api/agents/{id}/output", routing::get(list_output_files))
        .route(
            "/api/agents/{id}/output/{*path}",
            routing::get(serve_output_file),
        )
        // File explorer endpoints
        .route("/api/files/tree/{agent}", routing::get(list_files_tree))
        .route("/api/files/view/{agent}/{*path}", routing::get(view_file))
}
