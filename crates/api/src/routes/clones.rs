//! Clone (.agx) lifecycle endpoints.

use crate::routes::common::*;
use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;

// ========== Clone (.agx) endpoints ==========

/// GET /api/clones — List installed clones (agents with clone_source).
pub async fn list_clones(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let agents = state.kernel.registry.list();
    let clones: Vec<serde_json::Value> = agents
        .into_iter()
        .filter(|e| e.manifest.clone_source.is_some())
        .map(|e| {
            let cs = e.manifest.clone_source.as_ref().unwrap();
            serde_json::json!({
                "id": e.id.to_string(),
                "name": e.name,
                "state": format!("{:?}", e.state),
                "template_name": cs.template_name,
                "template_author": cs.template_author,
                "installed_at": cs.installed_at,
                "knowledge_files": e.manifest.knowledge_files,
                "skills": e.manifest.skills,
            })
        })
        .collect();

    (StatusCode::OK, Json(serde_json::json!(clones)))
}
/// POST /api/clones/{name}/start — Start a stopped clone.
pub async fn start_clone(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let entry = match state.kernel.registry.find_by_name(&name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Clone not found"})),
            )
        }
    };

    if entry.manifest.clone_source.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Not a clone agent"})),
        );
    }

    match state
        .kernel
        .registry
        .set_state(entry.id, types::agent::AgentState::Running)
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "running"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e}")})),
        ),
    }
}
/// POST /api/clones/{name}/stop — Stop a running clone.
pub async fn stop_clone(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let entry = match state.kernel.registry.find_by_name(&name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Clone not found"})),
            )
        }
    };

    if entry.manifest.clone_source.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Not a clone agent"})),
        );
    }

    match state
        .kernel
        .registry
        .set_state(entry.id, types::agent::AgentState::Suspended)
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "suspended"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e}")})),
        ),
    }
}
/// Run knowledge compile for a clone agent.
///
/// POST /api/clones/{name}/compile
///
/// Triggers metadata generation, overlap merging, stale/expiry cleanup,
/// and compression on the clone's knowledge directory.
pub async fn clone_compile(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let (entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Resolve an LLM driver for compile operations
    let driver = match state.kernel.resolve_driver(&entry.manifest) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::FAILED_DEPENDENCY,
                Json(serde_json::json!({"error": "No LLM driver available for compile"})),
            )
        }
    };

    let config = lifecycle::evolution_config::read_evolution_config(&workspace);

    // Run compile in a blocking thread with an async LLM callback
    let result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        let llm_call = |sys: &str, user: &str, max_tokens: u32| -> anyhow::Result<String> {
            let request = runtime::llm_driver::CompletionRequest {
                model: String::new(),
                messages: vec![types::message::Message {
                    role: types::message::Role::User,
                    content: types::message::MessageContent::Text(user.to_string()),
                }],
                tools: vec![],
                max_tokens,
                temperature: 0.3,
                system: Some(sys.to_string()),
                thinking: None,
                extra: Default::default(),
            };
            rt.block_on(async { driver.complete(request).await })
                .map(|r: runtime::llm_driver::CompletionResponse| r.text())
                .map_err(|e| anyhow::anyhow!("{e}"))
        };

        lifecycle::compile::run_compile(&workspace, &config, &llm_call)
    })
    .await;

    match result {
        Ok(result) => {
            tracing::info!(
                clone = %name,
                metadata = result.metadata_generated,
                merged = result.files_merged,
                stale = result.stale_marked,
                deleted = result.expired_deleted,
                compressed = result.files_compressed,
                skipped = result.skipped_unchanged,
                errors = result.errors.len(),
                "Compile complete"
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "compiled",
                    "metadata_generated": result.metadata_generated,
                    "files_merged": result.files_merged,
                    "stale_marked": result.stale_marked,
                    "expired_deleted": result.expired_deleted,
                    "files_compressed": result.files_compressed,
                    "skipped_unchanged": result.skipped_unchanged,
                    "errors": result.errors,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Compile task panicked: {e}")})),
        ),
    }
}
/// Run health check for a clone agent's knowledge directory.
///
/// GET /api/clones/{name}/health
///
/// Returns a health report with warnings and errors. Optionally auto-fixes
/// issues when `?fix=true` is passed.
pub async fn clone_health(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let (_entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let do_fix = params.get("fix").map(|v| v == "true").unwrap_or(false);

    let report = lifecycle::health::check_health(&workspace);

    if do_fix {
        let fixes = lifecycle::health::auto_fix(&workspace, &report);
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "report": report,
                "fixes_applied": fixes,
            })),
        );
    }

    (StatusCode::OK, Json(serde_json::json!({"report": report})))
}
/// Push collected feedback to Hub.
///
/// POST /api/clones/{name}/feedback/push
///
/// Collects all feedback entries from `feedback/*.json` and pushes them
/// to the configured Hub.
pub async fn clone_feedback_push(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let (_entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let entries = match lifecycle::feedback::collect_feedback(&workspace) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to collect feedback: {e}")})),
            )
        }
    };

    if entries.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"status": "no_feedback", "count": 0})),
        );
    }

    let hub_url = state.kernel.config.hub.url.clone();
    let hub_api_key =
        clone::hub::read_api_key(&state.kernel.config.hub.api_key_env).unwrap_or_default();

    match lifecycle::feedback::push_feedback_to_hub(&hub_url, &hub_api_key, &entries).await
    {
        Ok(results) => {
            let pushed = results.iter().filter(|r| r.starts_with("ok:")).count();
            let failed = results.len() - pushed;
            tracing::info!(
                clone = %name,
                pushed = pushed,
                failed = failed,
                "Feedback pushed to Hub"
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "pushed",
                    "total": entries.len(),
                    "pushed": pushed,
                    "failed": failed,
                    "results": results,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Push failed: {e}")})),
        ),
    }
}
/// Evaluate clone quality — deterministic metrics + optional LLM assessment.
///
/// GET /api/clones/{name}/evaluate?mode=deterministic|full
pub async fn clone_evaluate(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let (entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let metrics = lifecycle::evaluate::compute_deterministic_metrics(&workspace);

    let mode = params
        .get("mode")
        .map(|s| s.as_str())
        .unwrap_or("deterministic");

    if mode == "full" {
        // Full mode: generate test questions from knowledge, ask clone, judge answers.
        let knowledge_content = lifecycle::evaluate::read_knowledge_for_eval(&workspace);
        let mut questions: Vec<lifecycle::evaluate::EvalQuestion> = Vec::new();
        let mut avg_llm_score: Option<f32> = None;

        if !knowledge_content.is_empty() {
            if let Ok(driver) = state.kernel.resolve_driver(&entry.manifest) {
                let (sys_prompt, user_prompt) =
                    lifecycle::evaluate::build_test_questions_prompt(&knowledge_content);

                // Generate test questions
                let response_text = match driver
                    .complete(runtime::llm_driver::CompletionRequest {
                        model: String::new(), // driver uses its default
                        messages: vec![types::message::Message {
                            role: types::message::Role::User,
                            content: types::message::MessageContent::Text(user_prompt),
                        }],
                        tools: vec![],
                        max_tokens: 1024,
                        temperature: 0.7,
                        system: Some(sys_prompt),
                        thinking: None,
                        extra: Default::default(),
                    })
                    .await
                {
                    Ok(resp) => resp.text(),
                    Err(_) => String::new(),
                };

                let test_qs = lifecycle::evaluate::parse_test_questions(&response_text);

                if !test_qs.is_empty() {
                    let mut scores: Vec<f32> = Vec::new();
                    for q in &test_qs {
                        // Ask the clone
                        let answer_text = match driver
                            .complete(runtime::llm_driver::CompletionRequest {
                                model: String::new(), // driver uses its default
                                messages: vec![types::message::Message {
                                    role: types::message::Role::User,
                                    content: types::message::MessageContent::Text(
                                        q.clone(),
                                    ),
                                }],
                                tools: vec![],
                                max_tokens: 1024,
                                temperature: 0.3,
                                system: Some(
                                    "Answer the following question concisely.".to_string(),
                                ),
                                thinking: None,
                                extra: Default::default(),
                            })
                            .await
                        {
                            Ok(resp) => resp.text(),
                            Err(_) => continue,
                        };

                        // Judge the answer
                        let (j_sys, j_user) =
                            lifecycle::evaluate::build_judge_prompt(q, &answer_text);
                        let judge_text = match driver
                            .complete(runtime::llm_driver::CompletionRequest {
                                model: String::new(), // driver uses its default
                                messages: vec![types::message::Message {
                                    role: types::message::Role::User,
                                    content: types::message::MessageContent::Text(j_user),
                                }],
                                tools: vec![],
                                max_tokens: 256,
                                temperature: 0.0,
                                system: Some(j_sys),
                                thinking: None,
                                extra: Default::default(),
                            })
                            .await
                        {
                            Ok(resp) => resp.text(),
                            Err(_) => continue,
                        };

                        let (score, feedback) =
                            lifecycle::evaluate::parse_judge_response(&judge_text);
                        scores.push(score);
                        questions.push(lifecycle::evaluate::EvalQuestion {
                            question: q.clone(),
                            score,
                            feedback,
                        });
                    }
                    if !scores.is_empty() {
                        avg_llm_score = Some(scores.iter().sum::<f32>() / scores.len() as f32);
                    }
                }
            }
        }

        let report = lifecycle::evaluate::EvalReport {
            metrics,
            questions,
            avg_llm_score,
        };
        return (StatusCode::OK, Json(serde_json::json!(report)));
    }

    // Deterministic-only mode
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "metrics": metrics,
        })),
    )
}
/// Rollback a knowledge file to its previous version.
///
/// POST /api/clones/{name}/rollback
/// Body: { "filename": "refund-policy.md" }
pub async fn clone_rollback(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let filename = match body["filename"].as_str() {
        Some(f) => f.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Missing 'filename' field"})),
            )
        }
    };

    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid filename: path separators not allowed"})),
        );
    }

    let (_entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    match lifecycle::version::rollback_file(&workspace, &filename) {
        Ok(()) => {
            tracing::info!(clone = %name, file = %filename, "Knowledge file rolled back");
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "rolled_back", "filename": filename})),
            )
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("{e}")})),
        ),
    }
}
/// Verify (approve) the latest unverified version of a knowledge file.
///
/// POST /api/clones/{name}/verify
/// Body: { "filename": "refund-policy.md" }
pub async fn clone_verify(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let filename = match body["filename"].as_str() {
        Some(f) => f.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Missing 'filename' field"})),
            )
        }
    };

    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid filename: path separators not allowed"})),
        );
    }

    let (_entry, workspace) = match get_clone_workspace(&name, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    match lifecycle::version::verify_version(&workspace, &filename) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "verified", "filename": filename})),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/clones/{name}/upgrade — Upgrade a clone from hub to latest version.
pub async fn upgrade_clone(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Verify the agent exists
    let entry = match state.kernel.registry.find_by_name(&name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Clone not found"})),
            )
        }
    };

    // Must be a clone with hub_template_id
    if entry.manifest.clone_source.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Not a clone agent"})),
        );
    }
    if entry
        .manifest
        .clone_source
        .as_ref()
        .and_then(|cs| cs.hub_template_id.as_ref())
        .is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "Clone has no hub_template_id, cannot upgrade from hub"}),
            ),
        );
    }

    match state.kernel.clone_upgrade(&name).await {
        Ok(version) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "name": name,
                "version": version,
                "status": "upgraded"
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// DELETE /api/clones/{name} — Uninstall a clone.
pub async fn uninstall_clone(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let entry = match state.kernel.registry.find_by_name(&name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Clone not found"})),
            )
        }
    };

    if entry.manifest.clone_source.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Not a clone agent"})),
        );
    }

    let agent_id = entry.id;
    let workspace = entry.manifest.workspace.clone();

    match state.kernel.kill_agent(agent_id) {
        Ok(()) => {
            if let Some(ws) = workspace {
                let _ = std::fs::remove_dir_all(&ws);
            }
            tracing::info!("Clone '{}' uninstalled", name);
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "uninstalled"})),
            )
        }
        Err(e) => {
            tracing::warn!("Failed to kill clone agent: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("{e}")})),
            )
        }
    }
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/clones", routing::get(list_clones))
        .route("/api/clones/{name}", routing::delete(uninstall_clone))
        .route("/api/clones/{name}/compile", routing::post(clone_compile))
        .route(
            "/api/clones/{name}/evaluate?mode=deterministic|full",
            routing::get(clone_evaluate),
        )
        .route(
            "/api/clones/{name}/feedback/push",
            routing::post(clone_feedback_push),
        )
        .route("/api/clones/{name}/health", routing::get(clone_health))
        .route("/api/clones/{name}/rollback", routing::post(clone_rollback))
        .route("/api/clones/{name}/start", routing::post(start_clone))
        .route("/api/clones/{name}/stop", routing::post(stop_clone))
        .route("/api/clones/{name}/upgrade", routing::post(upgrade_clone))
        .route("/api/clones/{name}/verify", routing::post(clone_verify))
}
