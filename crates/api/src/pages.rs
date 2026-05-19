//! Server-side rendered dashboard pages using minijinja templates.
//!
//! Page routes are placed BEFORE the auth middleware in server.rs, so they
//! handle their own session checks. Unauthenticated users see the login page.

use axum::extract::{Path, State};
use axum::http::Request;
use axum::response::{Html, IntoResponse, Redirect};
use std::sync::Arc;

use crate::routes::state::AppState;
use crate::session_auth;

// ── Template engine ───────────────────────────────────────────────────────

lazy_static::lazy_static! {
    static ref TEMPLATES: minijinja::Environment<'static> = {
        let mut env = minijinja::Environment::new();
        env.add_template("base.html", include_str!("../templates/base.html")).unwrap();
        env.add_template("login.html", include_str!("../templates/login.html")).unwrap();
        env.add_template("overview.html", include_str!("../templates/overview.html")).unwrap();
        env.add_template("agents.html", include_str!("../templates/agents.html")).unwrap();
        env.add_template("chat.html", include_str!("../templates/chat.html")).unwrap();
        env.add_template("user_chat.html", include_str!("../templates/user_chat.html")).unwrap();
        env.add_template("tasks.html", include_str!("../templates/tasks.html")).unwrap();
        env.add_template("brain.html", include_str!("../templates/brain.html")).unwrap();
        env
    };
}

fn render(name: &str, ctx: minijinja::Value) -> Html<String> {
    let tmpl = TEMPLATES.get_template(name).expect("template exists");
    Html(tmpl.render(ctx).expect("template render"))
}

// ── Auth helpers ──────────────────────────────────────────────────────────

fn get_session_user(request: &Request<axum::body::Body>, state: &AppState) -> Option<String> {
    if !state.kernel.config.auth.enabled {
        return Some("admin".to_string());
    }
    let secret = &state.kernel.config.api_key;
    let session_token = request
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookie_str| {
            cookie_str
                .split(';')
                .find_map(|part| part.trim().strip_prefix("opencarrier_session="))
        });
    session_token.and_then(|token| {
        session_auth::verify_session_token(token, secret).map(|info| info.username)
    })
}

fn require_auth(
    request: &Request<axum::body::Body>,
    state: &AppState,
) -> Result<String, Html<String>> {
    match get_session_user(request, state) {
        Some(user) => Ok(user),
        None => Err(render(
            "login.html",
            minijinja::context! {
                hide_sidebar => true,
                version => env!("CARGO_PKG_VERSION"),
            },
        )),
    }
}

// ── Page handlers ─────────────────────────────────────────────────────────

/// GET /login — Login page (always public).
pub async fn login_page() -> impl IntoResponse {
    render(
        "login.html",
        minijinja::context! {
            hide_sidebar => true,
            version => env!("CARGO_PKG_VERSION"),
        },
    )
}

/// GET /logout — Clear session cookie and redirect to login.
pub async fn logout_page() -> impl IntoResponse {
    let mut response = Redirect::to("/login").into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        "opencarrier_session=; Path=/; Max-Age=0; SameSite=Strict"
            .parse()
            .unwrap(),
    );
    response
}

/// GET / — Overview dashboard.
pub async fn overview_page(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;
    let all_agents = state.kernel.registry.list();
    let running_count = all_agents
        .iter()
        .filter(|e| matches!(e.state, types::agent::AgentState::Running))
        .count();

    // Compute install counts per agent (default + clones across all senders)
    let install_counts: std::collections::HashMap<String, usize> =
        if let Some(ref pm_arc) = state.channel_manager {
            let pm = pm_arc.lock().await;
            pm.count_agents_per_sender()
        } else {
            std::collections::HashMap::new()
        };
    let total_installs: usize = install_counts.values().sum();

    // Compute user counts per agent from sessions (label = user:sender_id)
    let mut user_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for e in &all_agents {
        if let Ok(users) = state.kernel.memory.list_agent_users(&e.id.to_string()) {
            user_counts.insert(e.id.to_string(), users.len());
        }
    }
    let total_users: usize = user_counts.values().sum();

    let agents: Vec<minijinja::Value> = all_agents
        .into_iter()
        .map(|e| {
            let ready = matches!(e.state, types::agent::AgentState::Running);
            let (_, model) = state.kernel.resolve_model_label(&e.manifest.model.modality);
            let id_str = e.id.to_string();
            minijinja::context! {
                id => id_str,
                name => e.name,
                display_name => e.manifest.display_name,
                state => format!("{:?}", e.state),
                ready => ready,
                model_name => model,
                emoji => e.identity.emoji,
                installs => install_counts.get(&id_str).copied().unwrap_or(0),
                users => user_counts.get(&id_str).copied().unwrap_or(0),
            }
        })
        .collect();

    Ok(render(
        "overview.html",
        minijinja::context! {
            page => "overview",
            session_user => user,
            version => env!("CARGO_PKG_VERSION"),
            running_count => running_count,
            total_installs => total_installs,
            total_users => total_users,
            agents => agents,
        },
    ))
}

/// GET /agents — Clone detail page (shows users for a specific clone).
///
/// When no id param is given, redirects to overview. When id is provided,
/// shows the clone's install count, user count, and user list.
pub async fn agents_page(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;
    // No specific agent selected — redirect to overview
    let agents_list = state.kernel.registry.list();
    if agents_list.is_empty() {
        return Ok(render(
            "agents.html",
            minijinja::context! {
                page => "agents",
                session_user => user,
                version => env!("CARGO_PKG_VERSION"),
                agent => minijinja::context! {
                    id => "",
                    name => "未选择分身".to_string(),
                    display_name => "".to_string(),
                    state => "".to_string(),
                    ready => false,
                    model_name => "".to_string(),
                    emoji => None::<String>,
                },
                install_count => 0,
                user_count => 0,
                users => Vec::<minijinja::Value>::new(),
            },
        ));
    }
    // Show first agent by default
    let first = &agents_list[0];
    render_clone_detail(&state, &user, first).await
}

/// GET /agents/:id — Clone detail page for a specific agent.
pub async fn agent_detail_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;

    // Find agent by name or UUID
    let all_agents = state.kernel.registry.list();
    let entry = if let Ok(uid) = uuid::Uuid::parse_str(&id) {
        all_agents.into_iter().find(|e| e.id.0 == uid)
    } else {
        all_agents.into_iter().find(|e| e.name == id)
    };

    let entry = match entry {
        Some(e) => e,
        None => {
            return Ok(render(
                "agents.html",
                minijinja::context! {
                    page => "agents",
                    session_user => user,
                    version => env!("CARGO_PKG_VERSION"),
                    agent => minijinja::context! {
                        id => id,
                        name => "未找到".to_string(),
                        display_name => "".to_string(),
                        state => "".to_string(),
                        ready => false,
                        model_name => "".to_string(),
                        emoji => None::<String>,
                    },
                    install_count => 0,
                    user_count => 0,
                    users => Vec::<minijinja::Value>::new(),
                },
            ));
        }
    };

    render_clone_detail(&state, &user, &entry).await
}

/// Shared helper to render the clone detail page.
async fn render_clone_detail(
    state: &Arc<AppState>,
    user: &str,
    entry: &types::agent::AgentEntry,
) -> Result<Html<String>, Html<String>> {
    let agent_id_str = entry.id.to_string();
    let ready = matches!(entry.state, types::agent::AgentState::Running);
    let (_, model) = state.kernel.resolve_model_label(&entry.manifest.model.modality);

    // Install count (default + clones across all senders)
    let install_count: usize = if let Some(ref pm_arc) = state.channel_manager {
        let pm = pm_arc.lock().await;
        pm.count_agents_per_sender()
            .get(&agent_id_str)
            .copied()
            .unwrap_or(0)
    } else {
        0
    };

    // User list from sessions
    let users_raw = state
        .kernel
        .memory
        .list_agent_users(&agent_id_str)
        .unwrap_or_default();
    let user_count = users_raw.len();

    let users: Vec<minijinja::Value> = users_raw
        .iter()
        .map(|u| {
            let last_active = u["last_active"].as_str().unwrap_or("");
            let ago = format_time_ago(last_active);
            minijinja::context! {
                sender_id => u["sender_id"].as_str().unwrap_or(""),
                session_count => u["session_count"].as_i64().unwrap_or(0),
                last_active_ago => ago,
            }
        })
        .collect();

    Ok(render(
        "agents.html",
        minijinja::context! {
            page => "agents",
            session_user => user,
            version => env!("CARGO_PKG_VERSION"),
            agent => minijinja::context! {
                id => agent_id_str,
                name => entry.name,
                display_name => entry.manifest.display_name,
                state => format!("{:?}", entry.state),
                ready => ready,
                model_name => model,
                emoji => entry.identity.emoji,
            },
            install_count => install_count,
            user_count => user_count,
            users => users,
        },
    ))
}

/// Format an ISO timestamp as a human-readable "time ago" string.
fn format_time_ago(iso: &str) -> String {
    let then = chrono::DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.with_timezone(&chrono::Local))
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(&format!("{}T00:00:00Z", iso))
                .map(|dt| dt.with_timezone(&chrono::Local))
        });
    match then {
        Ok(t) => {
            let now = chrono::Local::now();
            let diff = now.signed_duration_since(t);
            if diff.num_seconds() < 60 {
                "刚刚".to_string()
            } else if diff.num_minutes() < 60 {
                format!("{}分钟前", diff.num_minutes())
            } else if diff.num_hours() < 24 {
                format!("{}小时前", diff.num_hours())
            } else {
                format!("{}天前", diff.num_days())
            }
        }
        Err(_) => iso.to_string(),
    }
}

/// GET /agents/:id/users/:sender_id — Read-only conversation history.
pub async fn user_chat_page(
    State(state): State<Arc<AppState>>,
    Path((id, sender_id)): Path<(String, String)>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;

    let all_agents = state.kernel.registry.list();
    let entry = if let Ok(uid) = uuid::Uuid::parse_str(&id) {
        all_agents.into_iter().find(|e| e.id.0 == uid)
    } else {
        all_agents.into_iter().find(|e| e.name == id)
    };

    let entry = match entry {
        Some(e) => e,
        None => {
            return Ok(render(
                "agents.html",
                minijinja::context! {
                    page => "agents",
                    session_user => user,
                    version => env!("CARGO_PKG_VERSION"),
                    agent => minijinja::context! {
                        id => id,
                        name => "未找到".to_string(),
                        display_name => "".to_string(),
                        state => "".to_string(),
                        ready => false,
                        model_name => "".to_string(),
                        emoji => None::<String>,
                    },
                    install_count => 0,
                    user_count => 0,
                    users => Vec::<minijinja::Value>::new(),
                },
            ));
        }
    };

    let agent_id_str = entry.id.to_string();

    // Load all sessions for this agent + sender
    let sessions = state
        .kernel
        .memory
        .list_user_sessions(&agent_id_str, &sender_id)
        .unwrap_or_default();

    // Flatten messages across sessions, keeping only user/assistant text
    let messages: Vec<minijinja::Value> = sessions
        .into_iter()
        .flat_map(|(_sid, msgs)| msgs)
        .filter_map(|msg| match msg.role {
            types::message::Role::User => {
                let text = match &msg.content {
                    types::message::MessageContent::Text(s) => s.clone(),
                    types::message::MessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            types::message::ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                if text.is_empty() {
                    None
                } else {
                    Some(minijinja::context! {
                        role => "user",
                        text => text,
                    })
                }
            }
            types::message::Role::Assistant => {
                let text = match &msg.content {
                    types::message::MessageContent::Text(s) => s.clone(),
                    types::message::MessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            types::message::ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                if text.is_empty() {
                    None
                } else {
                    Some(minijinja::context! {
                        role => "assistant",
                        text => text,
                    })
                }
            }
            types::message::Role::System => None,
        })
        .collect();

    Ok(render(
        "user_chat.html",
        minijinja::context! {
            page => "user_chat",
            session_user => user,
            version => env!("CARGO_PKG_VERSION"),
            agent => minijinja::context! {
                id => agent_id_str,
                name => entry.name,
                display_name => entry.manifest.display_name,
                emoji => entry.identity.emoji,
            },
            sender_id => sender_id,
            messages => messages,
        },
    ))
}

/// GET /tasks — Task scheduler page.
pub async fn tasks_page(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;
    Ok(render(
        "tasks.html",
        minijinja::context! {
            page => "tasks",
            session_user => user,
            version => env!("CARGO_PKG_VERSION"),
        },
    ))
}

/// GET /brain — Brain config page.
pub async fn brain_page(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;
    let brain = state.kernel.brain_info();
    let config = brain.config();
    let ready = brain.ready_endpoints();

    let providers: Vec<minijinja::Value> = config
        .providers
        .iter()
        .map(|(name, p)| {
            let has_key = !p.api_key_env.is_empty() && types::env::get_env(&p.api_key_env).is_some();
            minijinja::context! {
                name => name,
                api_key_env => p.api_key_env.clone(),
                auth_type => p.auth_type.clone(),
                params => p.params.clone(),
                has_key => has_key,
            }
        })
        .collect();

    let endpoints: Vec<minijinja::Value> = config
        .endpoints
        .iter()
        .map(|(name, ep)| {
            minijinja::context! {
                name => name,
                provider => ep.provider.clone(),
                model => ep.model.clone(),
                base_url => ep.base_url.clone(),
                format => ep.format.to_string(),
                ready => ready.contains(name),
            }
        })
        .collect();

    let modalities: Vec<minijinja::Value> = config
        .modalities
        .iter()
        .map(|(name, m)| {
            minijinja::context! {
                name => name,
                primary => m.primary.clone(),
                fallbacks => m.fallbacks.clone(),
                description => m.description.clone(),
            }
        })
        .collect();

    Ok(render(
        "brain.html",
        minijinja::context! {
            page => "brain",
            session_user => user,
            version => env!("CARGO_PKG_VERSION"),
            default_modality => config.default_modality.clone(),
            providers => providers,
            endpoints => endpoints,
            modalities => modalities,
            supported_formats => types::brain::SUPPORTED_FORMATS,
        },
    ))
}
