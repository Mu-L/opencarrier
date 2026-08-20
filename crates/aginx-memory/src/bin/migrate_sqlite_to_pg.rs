//! Thin wrapper around `aginx_memory::migrate::run_migration`.
//!
//! Usage:
//!   aginx-memory-migrate --sqlite ~/.opencarrier/opencarrier.db \
//!     --pg postgres://user@host/db \
//!     --content-src ~/.opencarrier/memory_tree/content \
//!     --content-dst /var/lib/aginx-memory/content
//!
//! See `migrate.rs` for the per-table migration logic and type conversions.

use std::path::PathBuf;

use tokio_postgres::NoTls;
use types::error::{CarrierError, CarrierResult};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aginx_memory_migrate=info".into()),
        )
        .init();

    let args = parse_args()?;
    tracing::info!(sqlite = %args.sqlite.display(), "opening SQLite (read-only)");
    let sqlite = rusqlite::Connection::open_with_flags(
        &args.sqlite,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    tracing::info!(pg = %args.pg, "connecting PG");
    let (mut pg, conn) = tokio_postgres::connect(&args.pg, NoTls).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    // Ensure the aginxMemory schema exists (idempotent).
    aginx_memory::migrations::runner()
        .run_async(&mut pg)
        .await
        .map_err(|e| anyhow::anyhow!("PG migration: {e}"))?;
    tracing::info!("PG schema ready");

    // Migrate all memory-class tables (with resume via migration_progress).
    aginx_memory::migrate::run_migration(&sqlite, &mut pg).await?;

    // Content files: copy recursively (Obsidian .md tree).
    if args.content_src.exists() {
        tracing::info!(
            src = %args.content_src.display(),
            dst = %args.content_dst.display(),
            "copying content files"
        );
        copy_dir_recursive(&args.content_src, &args.content_dst)?;
        tracing::info!("content files copied");
    } else {
        tracing::warn!(src = %args.content_src.display(), "content src missing, skipping");
    }

    // Clear the migrated job queue - old jobs would replay sealing over already-sealed trees.
    let cleared: u64 = pg
        .execute("DELETE FROM mem_tree_jobs", &[])
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    tracing::info!(
        cleared,
        "cleared mem_tree_jobs (historical chunks already in mem_tree_chunks)"
    );

    tracing::info!("migration complete");
    Ok(())
}

/// Recursive directory copy (preserves structure; caller points at a fresh dst).
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> CarrierResult<()> {
    std::fs::create_dir_all(dst).map_err(|e| CarrierError::Internal(e.to_string()))?;
    for entry in std::fs::read_dir(src).map_err(|e| CarrierError::Internal(e.to_string()))? {
        let entry = entry.map_err(|e| CarrierError::Internal(e.to_string()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry
            .file_type()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        if ft.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| CarrierError::Internal(e.to_string()))?;
        }
    }
    Ok(())
}

struct Args {
    sqlite: PathBuf,
    pg: String,
    content_src: PathBuf,
    content_dst: PathBuf,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut sqlite = None;
    let mut pg = None;
    let mut content_src = None;
    let mut content_dst = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let v = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing value for {a}"))?;
        match a.as_str() {
            "--sqlite" => sqlite = Some(PathBuf::from(v)),
            "--pg" => pg = Some(v),
            "--content-src" => content_src = Some(PathBuf::from(v)),
            "--content-dst" => content_dst = Some(PathBuf::from(v)),
            _ => return Err(anyhow::anyhow!("unknown arg {a}")),
        }
    }
    Ok(Args {
        sqlite: sqlite.ok_or_else(|| anyhow::anyhow!("--sqlite required"))?,
        pg: pg.ok_or_else(|| anyhow::anyhow!("--pg required"))?,
        content_src: content_src.unwrap_or_else(|| PathBuf::from("")),
        content_dst: content_dst.unwrap_or_else(|| PathBuf::from("")),
    })
}
