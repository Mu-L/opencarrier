//! aginxMemory - external kv+tree memory service.
//!
//! Standalone daemon backing opencarrier's memory subsystem with PostgreSQL +
//! Obsidian-compatible .md files. opencarrier delegates kv/tree operations to
//! this service over HTTP (see `HttpMemoryHandle` in the runtime crate);
//! sessions and other runtime state stay in opencarrier's in-process SQLite.
//!
//! Startup order: load config -> PG pool -> run migrations -> ensure
//! content_root -> start TreeWorkerPool (worker_count from config, default 0 =
//! queue-only) -> optionally start the daily scheduler -> axum::serve the
//! HTTP API (`server::build_router`).
//!
//! PG driver note: we use `tokio-postgres` + `refinery` (not `sqlx`) because
//! sqlx's dependency tree pulls `sqlx-sqlite` whose `libsqlite3-sys` conflicts
//! with `rusqlite`'s under cargo's single-`links` rule.

use std::sync::Arc;

use anyhow::Context;
use deadpool_postgres::Manager;
use tokio_postgres::NoTls;
use types::config::{home_dir, AginxMemoryConfig, KernelConfig};

use aginx_memory::jobs::scheduler::start_scheduler;
use aginx_memory::jobs::worker::TreeWorkerPool;
use aginx_memory::server::{build_router, AppState};

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
        "aginx_memory.database_url is not set - configure [aginx_memory] in config.toml \
         or set the DATABASE_URL env var",
    )?;
    let listen = cfg
        .listen
        .clone()
        .unwrap_or_else(|| "127.0.0.1:4300".to_string());

    tracing::info!(listen = %listen, "aginxMemory starting (PG backend)");

    // Apply migrations via a direct connection (refinery's run_async needs
    // `&mut tokio_postgres::Client`; the pool's ClientWrapper doesn't impl it).
    let (mut mig_client, mig_conn) = tokio_postgres::connect(&database_url, NoTls)
        .await
        .with_context(|| "Failed to connect to PostgreSQL at the configured URL")?;
    tokio::spawn(async move {
        if let Err(e) = mig_conn.await {
            tracing::error!(error = %e, "PG migration connection error");
        }
    });
    aginx_memory::migrations::runner()
        .run_async(&mut mig_client)
        .await
        .context("Failed to run PG migrations")?;
    tracing::info!("PG migrations applied");
    drop(mig_client);

    // Build the serving pool.
    let pg_cfg: tokio_postgres::Config = database_url
        .parse()
        .with_context(|| format!("invalid database_url: {database_url}"))?;
    let mgr = Manager::new(pg_cfg, NoTls);
    let pool = deadpool_postgres::Pool::builder(mgr)
        .max_size(16)
        .build()
        .context("Failed to build PG connection pool")?;

    // Ensure the content-root directory exists.
    let content_root = cfg
        .content_root
        .clone()
        .unwrap_or_else(|| home_dir().join("memory_tree").join("content"));
    std::fs::create_dir_all(&content_root)
        .with_context(|| format!("failed to create content_root: {}", content_root.display()))?;
    tracing::info!(content_root = %content_root.display(), "content root ready");

    // Start the tree-job worker pool (worker_count=0 = queue-only, no consumers).
    let worker_pool = Arc::new(TreeWorkerPool::new(
        pool.clone(),
        content_root.clone(),
        cfg.worker_count,
    ));
    worker_pool.clone().start().await;

    // Optionally start the daily digest / stale-flush scheduler.
    if cfg.scheduler_enabled {
        tracing::info!("aginxMemory daily scheduler enabled");
        start_scheduler(pool.clone(), content_root.clone());
    } else {
        tracing::info!("aginxMemory daily scheduler disabled (default)");
    }

    let state = AppState {
        pool,
        content_root,
        worker_pool,
    };
    let app = build_router(state);

    let addr: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid listen address: {listen}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "aginxMemory listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("installed Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("aginxMemory shutdown signal received");
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
