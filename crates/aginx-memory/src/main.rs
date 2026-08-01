//! aginxMemory — external kv+tree memory service.
//!
//! Standalone daemon backing opencarrier's memory subsystem with PostgreSQL +
//! Obsidian-compatible .md files. opencarrier delegates kv/tree operations to
//! this service over HTTP (see `HttpMemoryHandle` in the runtime crate);
//! sessions and other runtime state stay in opencarrier's in-process SQLite.
//!
//! Stage 1 skeleton: load config -> connect PG -> run migrations -> /health.
//! The PG pool and full HTTP API land in later stages.
//!
//! PG driver note: we use `tokio-postgres` + `refinery` (not `sqlx`) because
//! sqlx's dependency tree pulls `sqlx-sqlite` whose `libsqlite3-sys` conflicts
//! with `rusqlite`'s under cargo's single-`links` rule.

use anyhow::Context;
use axum::{routing::get, Router};
use std::net::SocketAddr;
use tokio_postgres::NoTls;
use types::config::{home_dir, AginxMemoryConfig, KernelConfig};

// refinery embeds the `migrations/` dir (V<n>__<name>.sql files) at compile time,
// generating a `migrations` module with a `runner()` function.
refinery::embed_migrations!("migrations");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aginx_memory=info,tower_http=warn,info".into()),
        )
        .init();

    let cfg = load_config();
    let database_url = cfg.database_url.clone().context(
        "aginx_memory.database_url is not set — configure [aginx_memory] in config.toml \
         or set the DATABASE_URL env var",
    )?;
    let listen = cfg
        .listen
        .clone()
        .unwrap_or_else(|| "127.0.0.1:4300".to_string());

    tracing::info!(listen = %listen, "aginxMemory starting (PG backend)");

    // Stage 1: single connection to apply migrations, then drop before serving.
    // Stage 4 replaces this with a deadpool-postgres Pool for the HTTP API.
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls)
        .await
        .with_context(|| "Failed to connect to PostgreSQL at the configured URL")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!(error = %e, "PG connection error");
        }
    });

    migrations::runner()
        .run_async(&mut client)
        .await
        .context("Failed to run PG migrations")?;
    tracing::info!("PG migrations applied");
    drop(client);

    let app = Router::new().route("/health", get(health));
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid listen address: {listen}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "aginxMemory listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

/// Load `[aginx_memory]` from `~/.opencarrier/config.toml`, with env overrides.
fn load_config() -> AginxMemoryConfig {
    let mut base = read_config_toml();
    if let Ok(url) = std::env::var("DATABASE_URL") {
        base.database_url = Some(url);
    }
    if let Ok(listen) = std::env::var("AGINX_MEMORY_LISTEN") {
        base.listen = Some(listen);
    }
    if let Ok(root) = std::env::var("CONTENT_ROOT") {
        base.content_root = Some(std::path::PathBuf::from(root));
    }
    if let Ok(s) = std::env::var("AGINX_MEMORY_WORKER_COUNT") {
        if let Ok(n) = s.parse() {
            base.worker_count = n;
        }
    }
    if let Ok(s) = std::env::var("AGINX_MEMORY_SCHEDULER") {
        base.scheduler_enabled = matches!(s.as_str(), "on" | "true" | "1");
    }
    base
}

fn read_config_toml() -> AginxMemoryConfig {
    let path = home_dir().join("config.toml");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return AginxMemoryConfig::default();
    };
    match toml::from_str::<KernelConfig>(&contents) {
        Ok(kc) => kc.aginx_memory,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to parse config.toml as KernelConfig; using default [aginx_memory]"
            );
            AginxMemoryConfig::default()
        }
    }
}
