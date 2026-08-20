//! SQLite -> PostgreSQL migration for aginxMemory (testable core).
//!
//! Extracted from the `aginx-memory-migrate` bin so the per-table migration
//! logic can be exercised by an env-gated integration test. The bin is a thin
//! wrapper that parses args, opens the SQLite file + PG connection, calls
//! [`run_migration`], then copies content files and clears the job queue.

use rusqlite::Row;
use serde_json::Value;
use types::error::{CarrierError, CarrierResult};

const BATCH_SIZE: usize = 1000;

/// Run the full table migration (kv + tree memory tables) with resume support.
/// `pg` must already have the aginxMemory schema applied (refinery migrations).
pub async fn run_migration(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<()> {
    pg.execute(
        "CREATE TABLE IF NOT EXISTS migration_progress (table_name TEXT PRIMARY KEY, rows BIGINT, done_at TEXT)",
        &[],
    )
    .await
    .map_err(|e| CarrierError::Memory(e.to_string()))?;

    macro_rules! run_table {
        ($pg:expr, $sqlite:expr, $name:expr, $f:expr) => {
            if $pg
                .query_opt(
                    "SELECT 1 FROM migration_progress WHERE table_name=$1",
                    &[&$name],
                )
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?
                .is_none()
            {
                let n = $f($sqlite, $pg).await?;
                let now = chrono::Utc::now().to_rfc3339();
                $pg.execute(
                    "INSERT INTO migration_progress (table_name, rows, done_at) VALUES ($1,$2,$3)",
                    &[&$name, &(n as i64), &now],
                )
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
                tracing::info!(table = $name, rows = n, "migrated");
            } else {
                tracing::info!(table = $name, "already migrated, skipping");
            }
        };
    }

    run_table!(pg, sqlite, "kv_store", migrate_kv_store);
    run_table!(pg, sqlite, "kv_history", migrate_kv_history);
    run_table!(pg, sqlite, "mem_tree_trees", migrate_trees);
    run_table!(pg, sqlite, "mem_tree_chunks", migrate_chunks);
    run_table!(pg, sqlite, "mem_tree_score", migrate_score);
    run_table!(pg, sqlite, "mem_tree_summaries", migrate_summaries);
    run_table!(pg, sqlite, "mem_tree_buffers", migrate_buffers);
    run_table!(pg, sqlite, "mem_tree_entity_index", migrate_entity_index);
    run_table!(
        pg,
        sqlite,
        "mem_tree_entity_hotness",
        migrate_entity_hotness
    );
    run_table!(
        pg,
        sqlite,
        "mem_tree_ingested_sources",
        migrate_ingested_sources
    );
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────
//
// These run inside `query_map` closures, which must return `rusqlite::Result`.
// Non-rusqlite failures (e.g. JSON parse) are mapped to
// `rusqlite::Error::FromSqlConversionFailure` so they propagate out of the row
// iterator and are converted to `CarrierError` at the call site.

fn txt(row: &Row, idx: usize) -> rusqlite::Result<String> {
    row.get(idx)
}
fn txt_opt(row: &Row, idx: usize) -> rusqlite::Result<Option<String>> {
    row.get(idx)
}
fn i64c(row: &Row, idx: usize) -> rusqlite::Result<i64> {
    row.get(idx)
}
fn i64_opt(row: &Row, idx: usize) -> rusqlite::Result<Option<i64>> {
    row.get(idx)
}
fn f64c(row: &Row, idx: usize) -> rusqlite::Result<f64> {
    row.get(idx)
}
fn f64_opt(row: &Row, idx: usize) -> rusqlite::Result<Option<f64>> {
    row.get(idx)
}
fn blob_opt(row: &Row, idx: usize) -> rusqlite::Result<Option<Vec<u8>>> {
    row.get(idx)
}

/// Parse a kv `value` BLOB (serde_json::to_vec output) into a JSONB Value.
/// On parse failure (corrupt row) store NULL and log - never abort the migration.
fn value_blob_to_jsonb(row: &Row, idx: usize) -> rusqlite::Result<Option<Value>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    Ok(match bytes {
        Some(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(error = %e, "kv value not valid JSON, storing NULL");
                None
            }
        },
        None => None,
    })
}

// ── per-table migrations ──────────────────────────────────────────────────

pub(crate) async fn migrate_kv_store(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare(
            "SELECT agent_id, owner_id, user_id, key, value, version, updated_at FROM kv_store",
        )
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                txt(r, 2)?,
                txt(r, 3)?,
                value_blob_to_jsonb(r, 4)?,
                i64c(r, 5)? as i32,
                txt(r, 6)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (agent_id, owner_id, user_id, key, value, version, updated_at) =
            row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO kv_store (agent_id, owner_id, user_id, key, value, version, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (agent_id, owner_id, user_id, key) DO NOTHING",
            &[&agent_id, &owner_id, &user_id, &key, &value, &version, &updated_at],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_kv_history(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    // kv_history may not exist on very old DBs; treat missing as 0 rows.
    let exists = sqlite
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='kv_history'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !exists {
        return Ok(0);
    }
    let mut stmt = sqlite
        .prepare(
            "SELECT agent_id, owner_id, user_id, key, value, version, archived_at FROM kv_history",
        )
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                txt(r, 2)?,
                txt(r, 3)?,
                value_blob_to_jsonb(r, 4)?,
                i64c(r, 5)? as i32,
                txt(r, 6)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (agent_id, owner_id, user_id, key, value, version, archived_at) =
            row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO kv_history (agent_id, owner_id, user_id, key, value, version, archived_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
            &[&agent_id, &owner_id, &user_id, &key, &value, &version, &archived_at],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_trees(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare("SELECT id, owner_id, user_id, kind, scope, root_id, max_level, status, created_at_ms, last_sealed_at_ms FROM mem_tree_trees")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                txt(r, 2)?,
                txt(r, 3)?,
                txt(r, 4)?,
                txt_opt(r, 5)?,
                i64c(r, 6)? as i32,
                txt(r, 7)?,
                i64c(r, 8)?,
                i64_opt(r, 9)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (
            id,
            owner_id,
            user_id,
            kind,
            scope,
            root_id,
            max_level,
            status,
            created_at_ms,
            last_sealed_at_ms,
        ) = row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_trees (id, owner_id, user_id, kind, scope, root_id, max_level, status, created_at_ms, last_sealed_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (id) DO NOTHING",
            &[&id, &owner_id, &user_id, &kind, &scope, &root_id, &max_level, &status, &created_at_ms, &last_sealed_at_ms],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_chunks(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    // user_id may be absent on pre-v27 DBs; COALESCE to ''.
    let mut stmt = sqlite
        .prepare("SELECT id, owner_id, COALESCE(user_id,''), agent_id, source_kind, source_id, source_ref, timestamp_ms, time_range_start_ms, time_range_end_ms, tags_json, content, token_count, seq_in_source, partial_message, lifecycle_status, created_at_ms FROM mem_tree_chunks")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                txt(r, 2)?,
                txt(r, 3)?,
                txt(r, 4)?,
                txt(r, 5)?,
                txt_opt(r, 6)?,
                i64c(r, 7)?,
                i64c(r, 8)?,
                i64c(r, 9)?,
                txt(r, 10)?,
                txt(r, 11)?,
                i64c(r, 12)? as i32,
                i64c(r, 13)? as i32,
                i64c(r, 14)? != 0,
                txt(r, 15)?,
                i64c(r, 16)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (
            id,
            owner_id,
            user_id,
            agent_id,
            source_kind,
            source_id,
            source_ref,
            timestamp_ms,
            trs,
            tre,
            tags_json,
            content,
            tc,
            sq,
            partial,
            lifecycle,
            created_at_ms,
        ) = row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_chunks (id, owner_id, user_id, agent_id, source_kind, source_id, source_ref, timestamp_ms, time_range_start_ms, time_range_end_ms, tags_json, content, token_count, seq_in_source, partial_message, lifecycle_status, created_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17) ON CONFLICT (id) DO NOTHING",
            &[&id, &owner_id, &user_id, &agent_id, &source_kind, &source_id, &source_ref, &timestamp_ms, &trs, &tre, &tags_json, &content, &tc, &sq, &partial, &lifecycle, &created_at_ms],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_score(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare("SELECT chunk_id, owner_id, total, token_count_signal, unique_words_signal, metadata_weight, source_weight, interaction_weight, entity_density, llm_importance, llm_importance_reason, dropped, reason, computed_at_ms FROM mem_tree_score")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                f64c(r, 2)?,
                f64c(r, 3)?,
                f64c(r, 4)?,
                f64c(r, 5)?,
                f64c(r, 6)?,
                f64c(r, 7)?,
                f64c(r, 8)?,
                f64c(r, 9)?,
                txt_opt(r, 10)?,
                i64c(r, 11)? != 0,
                txt_opt(r, 12)?,
                i64c(r, 13)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (
            chunk_id,
            owner_id,
            total,
            tcs,
            uws,
            mw,
            sw,
            iw,
            ed,
            li,
            lir,
            dropped,
            reason,
            computed_at_ms,
        ) = row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_score (chunk_id, owner_id, total, token_count_signal, unique_words_signal, metadata_weight, source_weight, interaction_weight, entity_density, llm_importance, llm_importance_reason, dropped, reason, computed_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) ON CONFLICT (chunk_id) DO NOTHING",
            &[&chunk_id, &owner_id, &total, &tcs, &uws, &mw, &sw, &iw, &ed, &li, &lir, &dropped, &reason, &computed_at_ms],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_summaries(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare("SELECT id, owner_id, COALESCE(user_id,''), tree_id, tree_kind, level, parent_id, child_ids_json, content, token_count, entities_json, topics_json, time_range_start_ms, time_range_end_ms, score, sealed_at_ms, deleted, embedding FROM mem_tree_summaries")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                txt(r, 2)?,
                txt(r, 3)?,
                txt(r, 4)?,
                i64c(r, 5)? as i32,
                txt_opt(r, 6)?,
                txt(r, 7)?,
                txt(r, 8)?,
                i64c(r, 9)? as i32,
                txt(r, 10)?,
                txt(r, 11)?,
                i64c(r, 12)?,
                i64c(r, 13)?,
                f64c(r, 14)?,
                i64c(r, 15)?,
                i64c(r, 16)? != 0,
                blob_opt(r, 17)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (
            id,
            owner_id,
            user_id,
            tree_id,
            tree_kind,
            level,
            parent_id,
            child_ids_json,
            content,
            tc,
            entities_json,
            topics_json,
            trs,
            tre,
            score,
            sealed_at_ms,
            deleted,
            embedding,
        ) = row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_summaries (id, owner_id, user_id, tree_id, tree_kind, level, parent_id, child_ids_json, content, token_count, entities_json, topics_json, time_range_start_ms, time_range_end_ms, score, sealed_at_ms, deleted, embedding) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18) ON CONFLICT (id) DO NOTHING",
            &[&id, &owner_id, &user_id, &tree_id, &tree_kind, &level, &parent_id, &child_ids_json, &content, &tc, &entities_json, &topics_json, &trs, &tre, &score, &sealed_at_ms, &deleted, &embedding],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_buffers(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare("SELECT tree_id, level, owner_id, item_ids_json, token_sum, oldest_at_ms, updated_at_ms FROM mem_tree_buffers")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                i64c(r, 1)? as i32,
                txt(r, 2)?,
                txt(r, 3)?,
                i64c(r, 4)? as i32,
                i64_opt(r, 5)?,
                i64c(r, 6)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (tree_id, level, owner_id, item_ids_json, token_sum, oldest_at_ms, updated_at_ms) =
            row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_buffers (tree_id, level, owner_id, item_ids_json, token_sum, oldest_at_ms, updated_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (tree_id, level) DO NOTHING",
            &[&tree_id, &level, &owner_id, &item_ids_json, &token_sum, &oldest_at_ms, &updated_at_ms],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_entity_index(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare("SELECT entity_id, node_id, node_kind, owner_id, COALESCE(user_id,''), entity_kind, surface, score, timestamp_ms, tree_id FROM mem_tree_entity_index")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                txt(r, 2)?,
                txt(r, 3)?,
                txt(r, 4)?,
                txt(r, 5)?,
                txt(r, 6)?,
                f64c(r, 7)?,
                i64c(r, 8)?,
                txt_opt(r, 9)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (
            entity_id,
            node_id,
            node_kind,
            owner_id,
            user_id,
            entity_kind,
            surface,
            score,
            timestamp_ms,
            tree_id,
        ) = row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_entity_index (entity_id, node_id, node_kind, owner_id, user_id, entity_kind, surface, score, timestamp_ms, tree_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (owner_id, entity_id, node_id) DO NOTHING",
            &[&entity_id, &node_id, &node_kind, &owner_id, &user_id, &entity_kind, &surface, &score, &timestamp_ms, &tree_id],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_entity_hotness(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare("SELECT entity_id, owner_id, mention_count_30d, distinct_sources, last_seen_ms, query_hits_30d, graph_centrality, ingests_since_check, last_hotness, last_updated_ms FROM mem_tree_entity_hotness")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                txt(r, 0)?,
                txt(r, 1)?,
                i64c(r, 2)? as i32,
                i64c(r, 3)? as i32,
                i64_opt(r, 4)?,
                i64c(r, 5)? as i32,
                f64_opt(r, 6)?,
                i64c(r, 7)? as i32,
                f64_opt(r, 8)?,
                i64c(r, 9)?,
            ))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (entity_id, owner_id, mc, ds, last_seen_ms, qh, gc, isc, lh, lum) =
            row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_entity_hotness (entity_id, owner_id, mention_count_30d, distinct_sources, last_seen_ms, query_hits_30d, graph_centrality, ingests_since_check, last_hotness, last_updated_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (owner_id, entity_id) DO NOTHING",
            &[&entity_id, &owner_id, &mc, &ds, &last_seen_ms, &qh, &gc, &isc, &lh, &lum],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

pub(crate) async fn migrate_ingested_sources(
    sqlite: &rusqlite::Connection,
    pg: &mut tokio_postgres::Client,
) -> CarrierResult<usize> {
    let mut stmt = sqlite
        .prepare("SELECT source_kind, source_id, owner_id, ingested_at_ms FROM mem_tree_ingested_sources")
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut n = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((txt(r, 0)?, txt(r, 1)?, txt(r, 2)?, i64c(r, 3)?))
        })
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    let mut tx = pg
        .transaction()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    for row in rows {
        let (source_kind, source_id, owner_id, ingested_at_ms) =
            row.map_err(|e| CarrierError::Memory(e.to_string()))?;
        tx.execute(
            "INSERT INTO mem_tree_ingested_sources (source_kind, source_id, owner_id, ingested_at_ms) \
             VALUES ($1,$2,$3,$4) ON CONFLICT (owner_id, source_kind, source_id) DO NOTHING",
            &[&source_kind, &source_id, &owner_id, &ingested_at_ms],
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
        n += 1;
        if n.is_multiple_of(BATCH_SIZE) {
            tx.commit()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
            tx = pg
                .transaction()
                .await
                .map_err(|e| CarrierError::Memory(e.to_string()))?;
        }
    }
    tx.commit()
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    use memory::MemorySubstrate;
    use serde_json::json;

    /// Build a synthetic opencarrier.db (memory schema + sample rows covering
    /// every type conversion), migrate it to PG, and verify row counts + types.
    #[tokio::test(flavor = "multi_thread")]
    async fn migrate_synthetic_db() {
        let Some(url) = std::env::var("AGINX_MEMORY_TEST_PG")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
            return;
        };

        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("opencarrier.db");
        // Create the memory schema (substrate runs all migrations on open).
        {
            let _s = MemorySubstrate::open(&db_path).unwrap();
        }

        // Insert sample rows covering type conversions.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let value_blob = serde_json::to_vec(&json!({"theme":"dark"})).unwrap();
        conn.execute(
            "INSERT INTO kv_store (agent_id, owner_id, user_id, key, value, version, updated_at) \
             VALUES ('a1','o1','u1','pref',?1,1,'2026-01-01')",
            rusqlite::params![&value_blob],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mem_tree_chunks \
             (id, owner_id, user_id, agent_id, source_kind, source_id, source_ref, \
              timestamp_ms, time_range_start_ms, time_range_end_ms, tags_json, content, \
              token_count, seq_in_source, partial_message, lifecycle_status, created_at_ms) \
             VALUES ('c1','o1','','a1','chat','wechat:s1',NULL,1700000000000,1700000000000,1700000000000,'[]','hello',5,0,1,'admitted',1700000000000)",
            [],
        )
        .unwrap();
        // Tree must exist before the summary (FK tree_id -> mem_tree_trees.id).
        conn.execute(
            "INSERT INTO mem_tree_trees \
             (id, owner_id, user_id, kind, scope, root_id, max_level, status, created_at_ms, last_sealed_at_ms) \
             VALUES ('t1','o1','','source','wechat:s1',NULL,0,'active',1700000000000,NULL)",
            [],
        )
        .unwrap();
        let emb = serde_json::to_vec(&vec![0.1_f32, 0.2, 0.3]).unwrap();
        conn.execute(
            "INSERT INTO mem_tree_summaries \
             (id, owner_id, user_id, tree_id, tree_kind, level, parent_id, child_ids_json, content, \
              token_count, entities_json, topics_json, time_range_start_ms, time_range_end_ms, score, \
              sealed_at_ms, deleted, embedding) \
             VALUES ('s1','o1','','t1','source',1,NULL,'[]','summary',10,'[]','[]',1700000000000,1700000000500,0.85,1700000120000,0,?1)",
            rusqlite::params![&emb],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mem_tree_score \
             (chunk_id, owner_id, total, token_count_signal, unique_words_signal, metadata_weight, \
              source_weight, interaction_weight, entity_density, llm_importance, llm_importance_reason, \
              dropped, reason, computed_at_ms) \
             VALUES ('c1','o1',0.75,0.5,0.5,0.5,0.5,0.5,0.5,0.0,NULL,0,'ok',1700000000000)",
            [],
        )
        .unwrap();

        // Reset PG to a clean schema.
        let (mut pg_reset, reset_conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = reset_conn.await;
        });
        crate::pg::reset_and_migrate(&mut pg_reset).await;
        drop(pg_reset);

        // Connect a fresh client, ensure schema, run migration.
        let (mut pg, conn_pg) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = conn_pg.await;
        });
        crate::migrations::runner()
            .run_async(&mut pg)
            .await
            .unwrap();

        run_migration(&conn, &mut pg).await.unwrap();

        // Verify counts.
        let kv: i64 = pg
            .query_one("SELECT COUNT(*) FROM kv_store", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(kv, 1, "kv_store");
        let chunks: i64 = pg
            .query_one("SELECT COUNT(*) FROM mem_tree_chunks", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(chunks, 1, "mem_tree_chunks");
        let sums: i64 = pg
            .query_one("SELECT COUNT(*) FROM mem_tree_summaries", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(sums, 1, "mem_tree_summaries");
        let scores: i64 = pg
            .query_one("SELECT COUNT(*) FROM mem_tree_score", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(scores, 1, "mem_tree_score");

        // Verify type conversions: kv value (BLOB->JSONB), chunk partial_message (1->bool), summary score (f64).
        let val: serde_json::Value = pg
            .query_one("SELECT value FROM kv_store WHERE key='pref'", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(val, json!({"theme":"dark"}));
        let partial: bool = pg
            .query_one(
                "SELECT partial_message FROM mem_tree_chunks WHERE id='c1'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(partial, "partial_message 1 -> true");
        let score: f64 = pg
            .query_one("SELECT score FROM mem_tree_summaries WHERE id='s1'", &[])
            .await
            .unwrap()
            .get(0);
        assert!((score - 0.85).abs() < 1e-9);
        // embedding round-trips as BYTEA (serde_json Vec<f32> bytes).
        let emb_back: Option<Vec<u8>> = pg
            .query_one(
                "SELECT embedding FROM mem_tree_summaries WHERE id='s1'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(emb_back.unwrap(), emb);

        // Resume: re-running is a no-op (progress recorded).
        let before: i64 = pg
            .query_one("SELECT COUNT(*) FROM migration_progress", &[])
            .await
            .unwrap()
            .get(0);
        drop(pg);
        let (mut pg2, conn2) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = conn2.await;
        });
        run_migration(&conn, &mut pg2).await.unwrap();
        let after: i64 = pg2
            .query_one("SELECT COUNT(*) FROM migration_progress", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(before, after, "resume skips already-done tables");
    }
}
