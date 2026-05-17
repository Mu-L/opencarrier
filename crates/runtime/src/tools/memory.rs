//! Tree memory tools — the primary memory interface for agents.
//!
//! Replaces the old flat KV/semantic/knowledge stores with hierarchical
//! tree-based memory (source → topic → global).

use crate::tool_context::ToolContext;
use crate::kernel_handle::KernelHandle;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct MemoryTools;

#[async_trait]
impl super::ToolModule for MemoryTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "memory_ingest".to_string(),
                description: "Ingest messages into the hierarchical memory tree. Typically called automatically after conversations — not usually needed directly.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "messages": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "sender": {"type": "string", "description": "Who sent the message"},
                                    "content": {"type": "string", "description": "The message text"},
                                    "timestamp_ms": {"type": "integer", "description": "Unix timestamp in milliseconds"},
                                },
                                "required": ["sender", "content", "timestamp_ms"],
                            },
                            "description": "Messages to ingest",
                        },
                        "source_kind": {"type": "string", "enum": ["chat", "email", "document"], "description": "Source type (default: chat)"},
                        "source_id": {"type": "string", "description": "Source identifier (e.g. wechat:gh_abc:sender_123)"},
                        "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags to attach"},
                    },
                    "required": ["messages"],
                }),
            },
            ToolDefinition {
                name: "memory_recall".to_string(),
                description: "Search your hierarchical memory. Returns relevant summaries and chunks from source and global trees. Use this to recall past conversations, decisions, and context.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "What to search for"},
                        "source_id": {"type": "string", "description": "Limit to a specific source (e.g. wechat:group_123)"},
                        "source_kind": {"type": "string", "enum": ["chat", "email", "document"], "description": "Filter by source type"},
                        "time_window_days": {"type": "integer", "description": "Only return memories from the last N days"},
                        "limit": {"type": "integer", "description": "Max results (default: 10)"},
                    },
                    "required": ["query"],
                }),
            },
            ToolDefinition {
                name: "memory_list".to_string(),
                description: "List all memory sources (conversations, emails, documents) that have been ingested. Shows source ID, type, and summary count.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source_kind": {"type": "string", "enum": ["chat", "email", "document"], "description": "Filter by source type"},
                        "limit": {"type": "integer", "description": "Max results (default: 20)"},
                    },
                }),
            },
            ToolDefinition {
                name: "memory_query_topic".to_string(),
                description: "Query memories related to a specific entity (person, email, topic, etc.). Returns summaries and chunks mentioning this entity across all sources.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "entity_id": {"type": "string", "description": "Entity to query (e.g. person:Alice, email:bob@example.com, topic:project-phoenix)"},
                        "time_window_days": {"type": "integer", "description": "Only return memories from the last N days"},
                        "limit": {"type": "integer", "description": "Max results (default: 10)"},
                    },
                    "required": ["entity_id"],
                }),
            },
            ToolDefinition {
                name: "memory_search_entities".to_string(),
                description: "Search for entities (people, emails, topics, etc.) in your memory. Returns matching entities with mention counts.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search term (matches entity IDs and surface forms)"},
                        "kind": {"type": "string", "enum": ["person", "email", "topic", "organization", "location", "event", "url", "technology", "artifact"], "description": "Filter by entity type"},
                        "limit": {"type": "integer", "description": "Max results (default: 5)"},
                    },
                    "required": ["query"],
                }),
            },
            ToolDefinition {
                name: "memory_drill_down".to_string(),
                description: "Navigate from a summary node to its children for more detail. Given a summary node_id, returns the next level of summaries or leaf chunks beneath it.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "node_id": {"type": "string", "description": "The summary node ID to drill into"},
                        "limit": {"type": "integer", "description": "Max results (default: 20)"},
                    },
                    "required": ["node_id"],
                }),
            },
            ToolDefinition {
                name: "memory_fetch_leaves".to_string(),
                description: "Fetch all raw leaf chunks under a summary node. Returns the original content that was summarized.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "node_id": {"type": "string", "description": "The summary node ID to fetch leaves for"},
                        "limit": {"type": "integer", "description": "Max results (default: 20)"},
                    },
                    "required": ["node_id"],
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
        let kernel = ctx.kernel?;
        let owner_id = ctx.owner_id.unwrap_or("default");
        let caller_agent_id = ctx.caller_agent_id.unwrap_or("");

        match name {
            "memory_ingest" => Some(tool_memory_ingest(input, kernel, owner_id, caller_agent_id).await),
            "memory_recall" => Some(tool_memory_recall(input, kernel, owner_id).await),
            "memory_list" => Some(tool_memory_list(input, kernel, owner_id).await),
            "memory_query_topic" => Some(tool_memory_query_topic(input, kernel, owner_id).await),
            "memory_search_entities" => Some(tool_memory_search_entities(input, kernel, owner_id).await),
            "memory_drill_down" => Some(tool_memory_drill_down(input, kernel, owner_id).await),
            "memory_fetch_leaves" => Some(tool_memory_fetch_leaves(input, kernel, owner_id).await),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "memory_ingest" => PermissionLevel::Write,
            "memory_recall" | "memory_list" | "memory_query_topic"
            | "memory_search_entities" | "memory_drill_down"
            | "memory_fetch_leaves" => PermissionLevel::None,
            _ => PermissionLevel::Dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

async fn tool_memory_ingest(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
    agent_id: &str,
) -> Result<String, String> {
    let messages_val = input["messages"].as_array().ok_or("messages must be an array")?;
    let mut messages = Vec::new();
    for m in messages_val {
        messages.push(types::memory_tree::IngestMessage {
            sender: m["sender"].as_str().unwrap_or("unknown").to_string(),
            content: m["content"].as_str().unwrap_or("").to_string(),
            timestamp_ms: m["timestamp_ms"].as_i64().unwrap_or(0),
        });
    }

    let source_kind = input["source_kind"].as_str().unwrap_or("chat").to_string();
    let source_id = input["source_id"].as_str().unwrap_or("api:manual").to_string();
    let tags: Vec<String> = input["tags"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let req = types::memory_tree::IngestRequest {
        owner_id: owner_id.to_string(),
        agent_id: agent_id.to_string(),
        source_kind,
        source_id,
        messages,
        tags,
    };

    let result = kernel.tree_ingest(req).await?;
    Ok(serde_json::to_string(&result).unwrap_or_else(|_| format!("Ingested {} chunks, dropped {}", result.chunks_created, result.chunks_dropped)))
}

async fn tool_memory_recall(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let query = input["query"].as_str().ok_or("query is required")?;
    let source_id = input["source_id"].as_str();
    let source_kind = input["source_kind"].as_str();
    let time_window_days = input["time_window_days"].as_u64().map(|d| d as u32);
    let limit = input["limit"].as_u64().unwrap_or(10) as usize;

    // Try global first, then source
    let global_req = types::memory_tree::GlobalQuery {
        owner_id,
        time_window_days,
        query: Some(query),
        limit,
    };

    let source_req = types::memory_tree::SourceQuery {
        owner_id,
        source_id,
        source_kind,
        time_window_days,
        query: Some(query),
        limit,
    };

    let (global_resp, source_resp) = tokio::join!(
        kernel.tree_query_global(global_req),
        kernel.tree_query_source(source_req),
    );

    let mut hits = Vec::new();

    if let Ok(resp) = global_resp {
        hits.extend(resp.hits);
    }
    if let Ok(resp) = source_resp {
        hits.extend(resp.hits);
    }

    // Deduplicate by node_id
    let mut seen = std::collections::BTreeSet::new();
    hits.retain(|h| seen.insert(h.node_id.clone()));

    // Truncate
    hits.truncate(limit);

    if hits.is_empty() {
        return Ok("No memories found matching your query.".to_string());
    }

    let mut lines = Vec::new();
    for hit in &hits {
        let kind = if hit.node_kind == types::memory_tree::NodeKind::Summary { "summary" } else { "chunk" };
        let time = format_time_range(hit.time_range_start_ms, hit.time_range_end_ms);
        lines.push(format!("[{}|{}|{}] {} (score: {:.2})", kind, hit.tree_scope, time, truncate_content(&hit.content, 200), hit.score));
    }
    Ok(lines.join("\n"))
}

async fn tool_memory_list(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let source_kind = input["source_kind"].as_str();
    let limit = input["limit"].as_u64().unwrap_or(20) as usize;

    let trees = kernel.tree_list_sources(owner_id, source_kind, limit).await?;

    if trees.is_empty() {
        return Ok("No memory sources found.".to_string());
    }

    let mut lines = Vec::new();
    for t in &trees {
        let sealed = t.last_sealed_at_ms
            .map(format_timestamp)
            .unwrap_or_else(|| "never".to_string());
        lines.push(format!(
            "- {} (kind: {}, scope: {}, summaries: {}, last sealed: {})",
            t.tree_id, t.kind, t.scope, t.summary_count, sealed
        ));
    }
    Ok(lines.join("\n"))
}

async fn tool_memory_query_topic(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let entity_id = input["entity_id"].as_str().ok_or("entity_id is required")?;
    let time_window_days = input["time_window_days"].as_u64().map(|d| d as u32);
    let limit = input["limit"].as_u64().unwrap_or(10) as usize;

    let req = types::memory_tree::TopicQuery {
        owner_id,
        entity_id,
        query: None,
        time_window_days,
        limit,
    };

    let resp = kernel.tree_query_topic(req).await?;

    if resp.hits.is_empty() {
        return Ok(format!("No memories found for entity '{}'.", entity_id));
    }

    let mut lines = Vec::new();
    for hit in &resp.hits {
        let kind = if hit.node_kind == types::memory_tree::NodeKind::Summary { "summary" } else { "chunk" };
        let time = format_time_range(hit.time_range_start_ms, hit.time_range_end_ms);
        lines.push(format!("[{}|{}|{}] {}", kind, hit.tree_scope, time, truncate_content(&hit.content, 200)));
    }
    Ok(lines.join("\n"))
}

async fn tool_memory_search_entities(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let query = input["query"].as_str().ok_or("query is required")?;
    let kind = input["kind"].as_str();
    let limit = input["limit"].as_u64().unwrap_or(5) as usize;

    let req = types::memory_tree::EntitySearch {
        owner_id,
        query,
        kind,
        limit,
    };

    let matches = kernel.tree_search_entities(req).await?;

    if matches.is_empty() {
        return Ok(format!("No entities matching '{}'.", query));
    }

    let mut lines = Vec::new();
    for m in &matches {
        lines.push(format!("- {} (kind: {}, mentions: {}, last seen: {})", m.canonical_id, m.kind, m.mention_count, format_timestamp(m.last_seen_ms)));
    }
    Ok(lines.join("\n"))
}

async fn tool_memory_drill_down(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let node_id = input["node_id"].as_str().ok_or("node_id is required")?;
    let limit = input["limit"].as_u64().unwrap_or(20) as usize;

    let req = types::memory_tree::DrillDownQuery {
        owner_id,
        node_id,
        limit,
    };

    let resp = kernel.tree_drill_down(req).await?;

    if resp.hits.is_empty() {
        return Ok(format!("No children found for node '{}'.", node_id));
    }

    let mut lines = Vec::new();
    for hit in &resp.hits {
        let kind = if hit.node_kind == types::memory_tree::NodeKind::Summary { "summary" } else { "chunk" };
        lines.push(format!("[{}|{}] {} (id: {})", kind, hit.level, truncate_content(&hit.content, 200), hit.node_id));
    }
    Ok(lines.join("\n"))
}

async fn tool_memory_fetch_leaves(
    input: &Value,
    kernel: &Arc<dyn KernelHandle>,
    owner_id: &str,
) -> Result<String, String> {
    let node_id = input["node_id"].as_str().ok_or("node_id is required")?;
    let limit = input["limit"].as_u64().unwrap_or(20) as usize;

    let req = types::memory_tree::FetchLeavesQuery {
        owner_id,
        node_id,
        limit,
    };

    let resp = kernel.tree_fetch_leaves(req).await?;

    if resp.hits.is_empty() {
        return Ok(format!("No leaf chunks found under node '{}'.", node_id));
    }

    let mut lines = Vec::new();
    for hit in &resp.hits {
        lines.push(format!("[leaf|{}] (id: {})", truncate_content(&hit.content, 300), hit.node_id));
    }
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use std::sync::Arc;

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
