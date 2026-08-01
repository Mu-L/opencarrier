-- aginxMemory initial schema.
--
-- Translated from the SQLite memory-class tables (crates/memory/src/migration.rs
-- v17 tree tables + v1/v9/v13/v14 kv + v27 per-user user_id). Runtime tables
-- (sessions/agents/cron/flow_runs/usage/...) are NOT here — they stay in
-- opencarrier's in-process SQLite. This file owns only kv + tree memory.
--
-- Type mapping: TEXT->TEXT, *_ms INTEGER->BIGINT, small ints->INT/SMALLINT,
-- flags(partial_message/dropped)->BOOLEAN, REAL->DOUBLE PRECISION,
-- value/payload/*_json BLOB/TEXT->JSONB, embedding BLOB->BYTEA,
-- AUTOINCREMENT->BIGSERIAL.
--
-- Per-user isolation (f49c8f3): chunks/trees/summaries/entity_index carry a
-- `user_id` column ('' = owner-shared legacy). Queries filter with
-- `(user_id = $N OR user_id = '')` — new data isolated, legacy recall preserved.
-- score/buffers/hotness/ingested_sources/jobs are owner-level aggregates and
-- intentionally have NO user_id (by design, see migration.rs:1152).

-- ── kv store (per-agent/owner/user/key) ─────────────────────────────────
CREATE TABLE IF NOT EXISTS kv_store (
    agent_id   TEXT NOT NULL,
    owner_id   TEXT NOT NULL DEFAULT '',
    user_id    TEXT NOT NULL DEFAULT '',
    key        TEXT NOT NULL,
    value      JSONB NOT NULL,
    version    INT  NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, owner_id, user_id, key)
);

CREATE TABLE IF NOT EXISTS kv_history (
    id          BIGSERIAL PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    owner_id    TEXT NOT NULL DEFAULT '',
    user_id     TEXT NOT NULL DEFAULT '',
    key         TEXT NOT NULL,
    value       JSONB NOT NULL,
    version     INT  NOT NULL,
    archived_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_kv_history_agent_key ON kv_history(agent_id, key);

-- ── tree: chunks ────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS mem_tree_chunks (
    id                  TEXT PRIMARY KEY,
    owner_id            TEXT NOT NULL,
    user_id             TEXT NOT NULL DEFAULT '',
    agent_id            TEXT NOT NULL,
    source_kind         TEXT NOT NULL,
    source_id           TEXT NOT NULL,
    source_ref          TEXT,
    timestamp_ms        BIGINT NOT NULL,
    time_range_start_ms BIGINT NOT NULL,
    time_range_end_ms   BIGINT NOT NULL,
    tags_json           TEXT  NOT NULL DEFAULT '[]',
    content             TEXT   NOT NULL,
    token_count         INT    NOT NULL,
    seq_in_source       INT    NOT NULL,
    partial_message     BOOLEAN NOT NULL DEFAULT FALSE,
    lifecycle_status    TEXT NOT NULL DEFAULT 'admitted',
    created_at_ms       BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_chunks_owner_user
    ON mem_tree_chunks(owner_id, user_id);
CREATE INDEX IF NOT EXISTS idx_mem_tree_chunks_owner_source
    ON mem_tree_chunks(owner_id, source_kind, source_id);
CREATE INDEX IF NOT EXISTS idx_mem_tree_chunks_owner_timestamp
    ON mem_tree_chunks(owner_id, timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_mem_tree_chunks_owner_lifecycle
    ON mem_tree_chunks(owner_id, lifecycle_status);
CREATE INDEX IF NOT EXISTS idx_mem_tree_chunks_source_seq
    ON mem_tree_chunks(owner_id, source_kind, source_id, seq_in_source);

-- ── tree: score (owner-level aggregate, no user_id) ────────────────────
CREATE TABLE IF NOT EXISTS mem_tree_score (
    chunk_id            TEXT PRIMARY KEY,
    owner_id            TEXT NOT NULL,
    total               DOUBLE PRECISION NOT NULL,
    token_count_signal  DOUBLE PRECISION NOT NULL,
    unique_words_signal DOUBLE PRECISION NOT NULL,
    metadata_weight     DOUBLE PRECISION NOT NULL,
    source_weight       DOUBLE PRECISION NOT NULL,
    interaction_weight  DOUBLE PRECISION NOT NULL,
    entity_density      DOUBLE PRECISION NOT NULL,
    llm_importance      DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    llm_importance_reason TEXT,
    dropped             BOOLEAN NOT NULL DEFAULT FALSE,
    reason              TEXT,
    computed_at_ms      BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_score_owner_total
    ON mem_tree_score(owner_id, total);
CREATE INDEX IF NOT EXISTS idx_mem_tree_score_owner_dropped
    ON mem_tree_score(owner_id, dropped);

-- ── tree: entity_index ─────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS mem_tree_entity_index (
    entity_id   TEXT NOT NULL,
    node_id     TEXT NOT NULL,
    node_kind   TEXT NOT NULL,
    owner_id    TEXT NOT NULL,
    user_id     TEXT NOT NULL DEFAULT '',
    entity_kind TEXT NOT NULL,
    surface     TEXT NOT NULL,
    score       DOUBLE PRECISION NOT NULL,
    timestamp_ms BIGINT NOT NULL,
    tree_id     TEXT,
    PRIMARY KEY (owner_id, entity_id, node_id)
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_entity_index_owner_user
    ON mem_tree_entity_index(owner_id, user_id);
CREATE INDEX IF NOT EXISTS idx_mem_tree_entity_index_owner_entity
    ON mem_tree_entity_index(owner_id, entity_id);
CREATE INDEX IF NOT EXISTS idx_mem_tree_entity_index_owner_node
    ON mem_tree_entity_index(owner_id, node_id);
CREATE INDEX IF NOT EXISTS idx_mem_tree_entity_index_owner_timestamp
    ON mem_tree_entity_index(owner_id, timestamp_ms);

-- ── tree: trees (hierarchy roots) ──────────────────────────────────────
CREATE TABLE IF NOT EXISTS mem_tree_trees (
    id                TEXT PRIMARY KEY,
    owner_id          TEXT NOT NULL,
    user_id           TEXT NOT NULL DEFAULT '',
    kind              TEXT NOT NULL,
    scope             TEXT NOT NULL,
    root_id           TEXT,
    max_level         INT NOT NULL DEFAULT 0,
    status            TEXT NOT NULL DEFAULT 'active',
    created_at_ms     BIGINT NOT NULL,
    last_sealed_at_ms BIGINT
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_trees_owner_user_kind
    ON mem_tree_trees(owner_id, user_id, kind);
CREATE UNIQUE INDEX IF NOT EXISTS idx_mem_tree_trees_owner_kind_scope
    ON mem_tree_trees(owner_id, kind, scope);
CREATE INDEX IF NOT EXISTS idx_mem_tree_trees_owner_status
    ON mem_tree_trees(owner_id, status);

-- ── tree: summaries ────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS mem_tree_summaries (
    id                  TEXT PRIMARY KEY,
    owner_id            TEXT NOT NULL,
    user_id             TEXT NOT NULL DEFAULT '',
    tree_id             TEXT NOT NULL,
    tree_kind           TEXT NOT NULL,
    level               INT NOT NULL,
    parent_id           TEXT,
    child_ids_json      TEXT NOT NULL DEFAULT '[]',
    content             TEXT NOT NULL,
    token_count         INT NOT NULL,
    entities_json       TEXT NOT NULL DEFAULT '[]',
    topics_json         TEXT NOT NULL DEFAULT '[]',
    time_range_start_ms BIGINT NOT NULL,
    time_range_end_ms   BIGINT NOT NULL,
    score               DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    sealed_at_ms        BIGINT NOT NULL,
    deleted             BOOLEAN NOT NULL DEFAULT FALSE,
    embedding           BYTEA DEFAULT NULL,
    FOREIGN KEY (tree_id) REFERENCES mem_tree_trees(id)
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_summaries_owner_user_tree
    ON mem_tree_summaries(owner_id, user_id, tree_id);
CREATE INDEX IF NOT EXISTS idx_mem_tree_summaries_owner_tree_level
    ON mem_tree_summaries(owner_id, tree_id, level);
CREATE INDEX IF NOT EXISTS idx_mem_tree_summaries_owner_parent
    ON mem_tree_summaries(owner_id, parent_id);
CREATE INDEX IF NOT EXISTS idx_mem_tree_summaries_owner_sealed_at
    ON mem_tree_summaries(owner_id, sealed_at_ms);
CREATE INDEX IF NOT EXISTS idx_mem_tree_summaries_owner_deleted
    ON mem_tree_summaries(owner_id, deleted);

-- ── tree: buffers (owner-level L0 admission buffers, no user_id) ───────
CREATE TABLE IF NOT EXISTS mem_tree_buffers (
    tree_id       TEXT NOT NULL,
    level         INT NOT NULL,
    owner_id      TEXT NOT NULL,
    item_ids_json TEXT NOT NULL DEFAULT '[]',
    token_sum     INT NOT NULL DEFAULT 0,
    oldest_at_ms  BIGINT,
    updated_at_ms BIGINT NOT NULL,
    PRIMARY KEY (tree_id, level),
    FOREIGN KEY (tree_id) REFERENCES mem_tree_trees(id)
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_buffers_owner_oldest
    ON mem_tree_buffers(owner_id, oldest_at_ms);

-- ── tree: entity_hotness (owner-level, no user_id) ─────────────────────
CREATE TABLE IF NOT EXISTS mem_tree_entity_hotness (
    entity_id            TEXT NOT NULL,
    owner_id             TEXT NOT NULL,
    mention_count_30d    INT NOT NULL DEFAULT 0,
    distinct_sources     INT NOT NULL DEFAULT 0,
    last_seen_ms         BIGINT,
    query_hits_30d       INT NOT NULL DEFAULT 0,
    graph_centrality     DOUBLE PRECISION,
    ingests_since_check  INT NOT NULL DEFAULT 0,
    last_hotness         DOUBLE PRECISION,
    last_updated_ms      BIGINT NOT NULL,
    PRIMARY KEY (owner_id, entity_id)
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_entity_hotness_owner_score
    ON mem_tree_entity_hotness(owner_id, last_hotness);

-- ── tree: jobs (owner-level async pipeline queue, no user_id) ──────────
CREATE TABLE IF NOT EXISTS mem_tree_jobs (
    id               TEXT PRIMARY KEY,
    owner_id         TEXT NOT NULL,
    kind             TEXT NOT NULL,
    payload_json     TEXT NOT NULL,
    dedupe_key       TEXT,
    status           TEXT NOT NULL DEFAULT 'ready',
    attempts         INT NOT NULL DEFAULT 0,
    max_attempts     INT NOT NULL DEFAULT 5,
    available_at_ms  BIGINT NOT NULL,
    locked_until_ms  BIGINT,
    last_error       TEXT,
    created_at_ms    BIGINT NOT NULL,
    started_at_ms    BIGINT,
    completed_at_ms  BIGINT
);
CREATE INDEX IF NOT EXISTS idx_mem_tree_jobs_owner_ready
    ON mem_tree_jobs(owner_id, status, available_at_ms);
CREATE INDEX IF NOT EXISTS idx_mem_tree_jobs_owner_kind
    ON mem_tree_jobs(owner_id, kind);
-- Partial unique index for active dedupe. PG supports partial indexes natively
-- (unlike SQLite's WHERE on a unique index, which it also supports — kept as-is).
CREATE UNIQUE INDEX IF NOT EXISTS idx_mem_tree_jobs_owner_dedupe_active
    ON mem_tree_jobs(owner_id, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND status IN ('ready', 'running');

-- ── tree: ingested_sources (owner-level dedupe of ingest, no user_id) ──
CREATE TABLE IF NOT EXISTS mem_tree_ingested_sources (
    source_kind    TEXT NOT NULL,
    source_id      TEXT NOT NULL,
    owner_id       TEXT NOT NULL,
    ingested_at_ms BIGINT NOT NULL,
    PRIMARY KEY (owner_id, source_kind, source_id)
);
