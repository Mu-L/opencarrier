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
        session_auth::verify_session_token(token, secret)
            .map(|info| info.username)
    })
}

fn require_auth(request: &Request<axum::body::Body>, state: &AppState) -> Result<String, Html<String>> {
    match get_session_user(request, state) {
        Some(user) => Ok(user),
        None => {
            Err(render("login.html", minijinja::context! {
                hide_sidebar => true,
                version => env!("CARGO_PKG_VERSION"),
            }))
        }
    }
}

// ── Page handlers ─────────────────────────────────────────────────────────

/// GET /login — Login page (always public).
pub async fn login_page() -> impl IntoResponse {
    render("login.html", minijinja::context! {
        hide_sidebar => true,
        version => env!("CARGO_PKG_VERSION"),
    })
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
    let agent_count = all_agents.len();
    let uptime = state.started_at.elapsed().as_secs();
    let (_default_modality, default_model) = state.kernel.resolve_model_label("chat");

    let agents: Vec<minijinja::Value> = all_agents
        .into_iter()
        .map(|e| {
            let ready = matches!(e.state, opencarrier_types::agent::AgentState::Running);
            let (_, model) = state.kernel.resolve_model_label(&e.manifest.model.modality);
            minijinja::context! {
                id => e.id.to_string(),
                name => e.name,
                display_name => e.manifest.display_name,
                state => format!("{:?}", e.state),
                ready => ready,
                model_name => model,
                created_at => e.created_at.to_rfc3339(),
            }
        })
        .collect();

    Ok(render("overview.html", minijinja::context! {
        page => "overview",
        session_user => user,
        version => env!("CARGO_PKG_VERSION"),
        agent_count => agent_count,
        uptime => format!("{}h {}m", uptime / 3600, (uptime % 3600) / 60),
        default_model => default_model,
        agents => agents,
    }))
}

/// GET /agents — Agent list.
pub async fn agents_page(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;
    let all_agents = state.kernel.registry.list();

    let agents: Vec<minijinja::Value> = all_agents
        .into_iter()
        .map(|e| {
            let ready = matches!(e.state, opencarrier_types::agent::AgentState::Running);
            let (_, model) = state.kernel.resolve_model_label(&e.manifest.model.modality);
            minijinja::context! {
                id => e.id.to_string(),
                name => e.name,
                display_name => e.manifest.display_name,
                state => format!("{:?}", e.state),
                ready => ready,
                mode => format!("{:?}", e.mode),
                model_name => model,
                identity => minijinja::context! {
                    emoji => e.identity.emoji,
                },
            }
        })
        .collect();

    Ok(render("agents.html", minijinja::context! {
        page => "agents",
        session_user => user,
        version => env!("CARGO_PKG_VERSION"),
        agents => agents,
    }))
}

/// GET /agents/:id/chat — Chat with a specific agent.
pub async fn chat_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;

    let agent_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => {
            return Ok(render("agents.html", minijinja::context! {
                page => "agents",
                session_user => user,
                version => env!("CARGO_PKG_VERSION"),
                agents => Vec::<minijinja::Value>::new(),
            }));
        }
    };

    let entry = match state.kernel.registry.get(opencarrier_types::agent::AgentId(agent_id)) {
        Some(e) => e,
        None => {
            return Ok(render("agents.html", minijinja::context! {
                page => "agents",
                session_user => user,
                version => env!("CARGO_PKG_VERSION"),
                agents => Vec::<minijinja::Value>::new(),
            }));
        }
    };

    let (_, model) = state.kernel.resolve_model_label(&entry.manifest.model.modality);
    let api_key = state.kernel.config.api_key.clone();

    Ok(render("chat.html", minijinja::context! {
        page => "chat",
        session_user => user,
        version => env!("CARGO_PKG_VERSION"),
        agent => minijinja::context! {
            id => entry.id.to_string(),
            name => entry.name.clone(),
            state => format!("{:?}", entry.state),
            model_name => model,
            identity => minijinja::context! {
                emoji => entry.identity.emoji.clone(),
            },
        },
        api_key => api_key,
    }))
}

/// GET /tasks — Task scheduler page.
pub async fn tasks_page(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Result<Html<String>, Html<String>> {
    let user = require_auth(&request, &state)?;
    Ok(render("tasks.html", minijinja::context! {
        page => "tasks",
        session_user => user,
        version => env!("CARGO_PKG_VERSION"),
    }))
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
            let has_key = !p.api_key_env.is_empty()
                && std::env::var(&p.api_key_env).is_ok();
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

    Ok(render("brain.html", minijinja::context! {
        page => "brain",
        session_user => user,
        version => env!("CARGO_PKG_VERSION"),
        default_modality => config.default_modality.clone(),
        providers => providers,
        endpoints => endpoints,
        modalities => modalities,
    }))
}
