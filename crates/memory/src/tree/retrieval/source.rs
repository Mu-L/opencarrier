//! Per-source summary retrieval.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use types::error::CarrierResult;
use types::memory_tree::{NodeKind, QueryResponse, RetrievalHit, TreeKind, TreeSummary};

use crate::tree::tree_store::TreeTreeStore;
use crate::tree::types::SourceKind;

const DEFAULT_LIMIT: usize = 10;

/// Query source tree summaries.
pub fn query_source(
    conn: &Arc<Mutex<Connection>>,
    owner_id: &str,
    source_id: Option<&str>,
    source_kind: Option<SourceKind>,
    time_window_days: Option<u32>,
    limit: usize,
) -> CarrierResult<QueryResponse> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let tree_store = TreeTreeStore::new(conn.clone());

    let trees = select_trees(&tree_store, owner_id, source_id, source_kind)?;
    let mut hits: Vec<RetrievalHit> = Vec::new();

    for tree in &trees {
        if tree.max_level == 0 {
            continue;
        }
        for level in 1..=tree.max_level {
            let summaries = tree_store.list_summaries(owner_id, &tree.tree_id, Some(level), 100)?;
            for node in summaries {
                hits.push(RetrievalHit {
                    node_id: node.id,
                    node_kind: NodeKind::Summary,
                    tree_id: tree.tree_id.clone(),
                    tree_kind: TreeKind::Source,
                    tree_scope: tree.scope.clone(),
                    level,
                    content: node.content,
                    entities: node.entities,
                    topics: node.topics,
                    time_range_start_ms: node.time_range_start_ms,
                    time_range_end_ms: node.time_range_end_ms,
                    score: node.score,
                    child_ids: node.child_ids,
                    source_ref: None,
                });
            }
        }
    }

    if let Some(days) = time_window_days {
        hits = filter_by_window(hits, days);
    }

    let total = hits.len();

    // Sort newest-first
    hits.sort_by(|a, b| b.time_range_end_ms.cmp(&a.time_range_end_ms));
    hits.truncate(limit);

    Ok(QueryResponse {
        hits,
        total,
        truncated: total > limit,
    })
}

fn select_trees(
    tree_store: &TreeTreeStore,
    owner_id: &str,
    source_id: Option<&str>,
    source_kind: Option<SourceKind>,
) -> CarrierResult<Vec<TreeSummary>> {
    if let Some(id) = source_id {
        let trees = tree_store.list_trees(owner_id, Some(TreeKind::Source), 1000)?;
        return Ok(trees.into_iter().filter(|t| t.scope == id).collect());
    }
    let all = tree_store.list_trees(owner_id, Some(TreeKind::Source), 1000)?;
    if let Some(kind) = source_kind {
        let prefix = kind.as_str();
        return Ok(all
            .into_iter()
            .filter(|t| scope_matches_kind(&t.scope, prefix))
            .collect());
    }
    Ok(all)
}

fn scope_matches_kind(scope: &str, kind_prefix: &str) -> bool {
    let lower = scope.to_lowercase();
    if lower.starts_with(&format!("{kind_prefix}:")) {
        return true;
    }
    // Platform-specific prefix mapping
    const PLATFORM_KINDS: &[(&str, &str)] = &[
        ("wechat", "chat"), ("feishu", "chat"), ("wecom", "chat"),
        ("dingtalk", "chat"), ("slack", "chat"), ("api", "chat"),
        ("imap", "email"), ("gmail", "email"), ("outlook", "email"),
        ("notion", "document"), ("drive", "document"),
    ];
    PLATFORM_KINDS
        .iter()
        .any(|(platform, kind)| *kind == kind_prefix && lower.starts_with(&format!("{platform}:")))
}

fn filter_by_window(hits: Vec<RetrievalHit>, window_days: u32) -> Vec<RetrievalHit> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_start_ms = now_ms - (window_days as i64 * 86_400_000);
    hits.into_iter()
        .filter(|h| h.time_range_end_ms >= window_start_ms && h.time_range_start_ms <= now_ms)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use crate::tree::bucket_seal::BucketSealEngine;
    use crate::tree::summariser::inert::InertSummariser;
    use crate::tree::types::{Chunk, SourceKind};
    use crate::tree::store::ChunkStore;
    use tempfile::TempDir;

    fn setup() -> (Arc<Mutex<Connection>>, TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let dir = TempDir::new().unwrap();
        (Arc::new(Mutex::new(conn)), dir)
    }

    #[test]
    fn test_empty_owner_returns_empty() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let resp = query_source(&conn, "owner_x", None, None, None, 10)?;
        assert!(resp.hits.is_empty());
        assert_eq!(resp.total, 0);
        Ok(())
    }

    #[test]
    fn test_query_by_source_id() -> CarrierResult<()> {
        let (conn, _dir) = setup();
        let tree_store = TreeTreeStore::new(conn.clone());
        let chunk_store = ChunkStore::new(conn.clone());

        let tree = tree_store.get_or_create_tree("owner_1", TreeKind::Source, "wechat:test:sender")?;

        // Insert enough chunks and force seal
        for i in 0..10 {
            let chunk = Chunk {
                id: format!("chunk_src_{i}"),
                owner_id: "owner_1".to_string(),
                agent_id: "agent_1".to_string(),
                source_kind: SourceKind::Chat,
                source_id: "wechat:test:sender".to_string(),
                source_ref: None,
                timestamp_ms: 1_700_000_000_000,
                time_range_start_ms: 1_700_000_000_000,
                time_range_end_ms: 1_700_000_000_000,
                tags_json: "[]".to_string(),
                content: "test content for source query".to_string(),
                token_count: 6000,
                seq_in_source: i,
                partial_message: false,
                lifecycle_status: "admitted".to_string(),
                created_at_ms: 1_700_000_000_000,
            };
            chunk_store.upsert_chunks(&[chunk])?;
        }

        let seal_engine = BucketSealEngine::new(conn.clone(), _dir.path().to_path_buf(), Arc::new(InertSummariser));
        for i in 0..10 {
            seal_engine.append_to_buffer("owner_1", &tree.id, 0, &format!("chunk_src_{i}"), 6000, 1_700_000_000_000)?;
        }
        seal_engine.cascade_seals("owner_1", &tree, 0, false)?;

        let resp = query_source(&conn, "owner_1", Some("wechat:test:sender"), None, None, 10)?;
        assert!(!resp.hits.is_empty());
        assert_eq!(resp.hits[0].tree_scope, "wechat:test:sender");
        Ok(())
    }

    #[test]
    fn test_scope_matches_kind() {
        assert!(scope_matches_kind("wechat:abc", "chat"));
        assert!(scope_matches_kind("chat:custom", "chat"));
        assert!(!scope_matches_kind("wechat:abc", "email"));
    }
}
