//! Consolidated memory_tree tool — dispatches to the correct retrieval primitive
//! based on the `mode` argument. Reduces tool surface from 7 entries to 1.

use crate::tool_context::ToolContext;
use crate::kernel_handle::KernelHandle;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;
use std::sync::Arc;

pub struct MemoryTools;

#[async_trait]
impl super::ToolModule for MemoryTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "memory_tree".to_string(),
                description: "Query the user's hierarchical memory tree. This is a retrospective index of already-ingested conversations, emails, and documents — NOT a live API for connected services.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": [
                                "search_entities",
                                "query_topic",
                                "query_source",
                                "query_global",
                                "drill_down",
                                "fetch_leaves",
                            ],
                            "description": "Which retrieval operation to run",
                        },
                        "query": {
                            "type": "string",
                            "description": "Search term (used by search_entities, query_topic, query_source, query_global)",
                        },
                        "entity_id": {
                            "type": "string",
                            "description": "Canonical entity ID from search_entities (used by query_topic)",
                        },
                        "source_id": {
                            "type": "string",
                            "description": "Source identifier to filter by (used by query_source)",
                        },
                        "source_kind": {
                            "type": "string",
                            "enum": ["chat", "email", "document"],
                            "description": "Source type filter (used by query_source)",
                        },
                        "kinds": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Entity kind filter (used by search_entities), e.g. [\"person\", \"email\"]",
                        },
                        "time_window_days": {
                            "type": "integer",
                            "description": "Only return memories from the last N days (used by query_source, query_global, query_topic)",
                        },
                        "node_id": {
                            "type": "string",
                            "description": "Summary node ID to expand (used by drill_down)",
                        },
                        "max_depth": {
                            "type": "integer",
                            "description": "Levels to walk in drill_down (default: 1, max: 3)",
                        },
                        "chunk_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Chunk IDs to hydrate (used by fetch_leaves, max 20)",
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results to return (default varies by mode)",
                        },
                    },
                    "required": ["mode"],
                }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "memory_tree" {
            return None;
        }

        let kernel = match ctx.kernel {
            Some(k) => k,
            None => return Some(Err("memory_tree: kernel not available".to_string())),
        };
        let owner_id = ctx.owner_id.unwrap_or("default");

        let mode = match input.get("mode").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => return Some(Err("memory_tree: 'mode' parameter is required. Valid modes: search_entities, query_topic, query_source, query_global, drill_down, fetch_leaves".to_string())),
        };
        match mode {
            "search_entities" => Some(handle_search_entities(input, kernel, owner_id).await),
            "query_topic" => Some(handle_query_topic(input, kernel, owner_id).await),
            "query_source" => Some(handle_query_source(input, kernel, owner_id).await),
            "query_global" => Some(handle_query_global(input, kernel, owner_id).await),
            "drill_down" => Some(handle_drill_down(input, kernel, owner_id).await),
            "fetch_leaves" => Some(handle_fetch_leaves(input, kernel, owner_id).await),
            other => Some(Err(format!("memory_tree: unknown mode `{other}`. Valid modes: search_entities, query_topic, query_source, query_global, drill_down, fetch_leaves"))),
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "memory_tree" => PermissionLevel::None,
            _ => PermissionLevel::Dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// Mode handlers
// ---------------------------------------------------------------------------

async fn handle_search_entities(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let query = input["query"].as_str().ok_or("query is required for search_entities")?;
    let kind = input["kind"].as_str();
    let limit = input["limit"].as_u64().unwrap_or(5) as usize;

    let kinds: Option<&str> = input["kinds"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .or(kind);

    let req = types::memory_tree::EntitySearch {
        owner_id,
        query,
        kind: kinds,
        limit,
    };

    let matches = kernel.tree_search_entities(req).await?;

    if matches.is_empty() {
        return Ok(format!("No entities matching '{}'.", query));
    }

    let mut lines = Vec::new();
    for m in &matches {
        lines.push(format!(
            "- {} (kind: {}, mentions: {}, last seen: {})",
            m.canonical_id, m.kind, m.mention_count, format_timestamp(m.last_seen_ms)
        ));
    }
    Ok(lines.join("\n"))
}

async fn handle_query_topic(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let entity_id = input["entity_id"].as_str().ok_or("entity_id is required for query_topic")?;
    let time_window_days = input["time_window_days"].as_u64().map(|d| d as u32);
    let query = input["query"].as_str();
    let limit = input["limit"].as_u64().unwrap_or(10) as usize;

    let req = types::memory_tree::TopicQuery {
        owner_id,
        entity_id,
        query,
        time_window_days,
        limit,
    };

    let resp = kernel.tree_query_topic(req).await?;
    format_hit_response(resp)
}

async fn handle_query_source(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let source_id = input["source_id"].as_str();
    let source_kind = input["source_kind"].as_str();
    let time_window_days = input["time_window_days"].as_u64().map(|d| d as u32);
    let query = input["query"].as_str();
    let limit = input["limit"].as_u64().unwrap_or(10) as usize;

    let req = types::memory_tree::SourceQuery {
        owner_id,
        source_id,
        source_kind,
        time_window_days,
        query,
        limit,
    };

    let resp = kernel.tree_query_source(req).await?;
    format_hit_response(resp)
}

async fn handle_query_global(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let time_window_days = input["time_window_days"].as_u64().map(|d| d as u32);
    let query = input["query"].as_str();
    let limit = input["limit"].as_u64().unwrap_or(10) as usize;

    let req = types::memory_tree::GlobalQuery {
        owner_id,
        time_window_days,
        query,
        limit,
    };

    let resp = kernel.tree_query_global(req).await?;
    format_hit_response(resp)
}

async fn handle_drill_down(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let node_id = input["node_id"].as_str().ok_or("node_id is required for drill_down")?;
    let max_depth = input["max_depth"].as_u64().unwrap_or(1) as u32;
    let limit = input["limit"].as_u64().unwrap_or(20) as usize;

    let req = types::memory_tree::DrillDownQuery {
        owner_id,
        node_id,
        max_depth,
        limit,
    };

    let resp = kernel.tree_drill_down(req).await?;

    if resp.hits.is_empty() {
        return Ok(format!("No children found for node '{}'.", node_id));
    }

    let mut lines = Vec::new();
    for hit in &resp.hits {
        let kind = if hit.node_kind == types::memory_tree::NodeKind::Summary { "summary" } else { "chunk" };
        lines.push(format!(
            "[{}|L{}] {} (id: {}, children: [{}])",
            kind,
            hit.level,
            truncate_content(&hit.content, 200),
            hit.node_id,
            hit.child_ids.join(", ")
        ));
    }
    Ok(lines.join("\n"))
}

async fn handle_fetch_leaves(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let chunk_ids: Vec<String> = input["chunk_ids"]
        .as_array()
        .ok_or("chunk_ids is required for fetch_leaves and must be an array")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if chunk_ids.is_empty() {
        return Err("chunk_ids must not be empty".to_string());
    }

    let limit = input["limit"].as_u64().unwrap_or(20) as usize;

    let req = types::memory_tree::FetchLeavesQuery {
        owner_id,
        chunk_ids,
        limit,
    };

    let resp = kernel.tree_fetch_leaves(req).await?;

    if resp.hits.is_empty() {
        return Ok("No leaf chunks found for the given IDs.".to_string());
    }

    let mut lines = Vec::new();
    for hit in &resp.hits {
        lines.push(format!("[leaf|{}] (id: {})", truncate_content(&hit.content, 300), hit.node_id));
    }
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Shared formatting helpers
// ---------------------------------------------------------------------------

fn format_hit_response(resp: types::memory_tree::QueryResponse) -> Result<String, String> {
    if resp.hits.is_empty() {
        return Ok("No memories found matching your query. This query has been checked thoroughly — do not retry with the same query. Try a different query or proceed without this information.".to_string());
    }

    let mut lines = Vec::new();
    for hit in &resp.hits {
        let kind = if hit.node_kind == types::memory_tree::NodeKind::Summary { "summary" } else { "chunk" };
        let time = format_time_range(hit.time_range_start_ms, hit.time_range_end_ms);
        let children = if hit.child_ids.is_empty() {
            String::new()
        } else {
            format!(" children:[{}]", hit.child_ids.join(","))
        };
        lines.push(format!(
            "[{}|{}|{}] {} (id: {}, score: {:.2}{})",
            kind,
            hit.tree_scope,
            time,
            truncate_content(&hit.content, 200),
            hit.node_id,
            hit.score,
            children
        ));
    }
    Ok(lines.join("\n"))
}

fn truncate_content(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}...", &s[..max]) }
}

fn format_timestamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ms.to_string())
}

fn format_time_range(start_ms: i64, end_ms: i64) -> String {
    format!("{} — {}", format_timestamp(start_ms), format_timestamp(end_ms))
}
