//! Cron scheduler for declarative API tools.
//!
//! Tools with [tool.cron] are executed on a schedule without involving any
//! LLM. Results are stored directly to SQLite — zero token cost.

use types::api_tool::{ApiToolDef, ApiCronDef};
use std::path::PathBuf;
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Clone)]
struct ApiCronEntry {
    tool: ApiToolDef,
    cron: ApiCronDef,
}

static CRON_ENTRIES: once_cell::sync::Lazy<RwLock<Vec<ApiCronEntry>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(Vec::new()));

/// Register all api_tools that have a [tool.cron] section. Starts background loop.
pub async fn register_cron_tools(tools: Vec<ApiToolDef>, home_dir: PathBuf) {
    let mut entries = Vec::new();
    for tool in tools {
        if let Some(ref cron) = tool.cron {
            info!(tool = %tool.name, schedule = %cron.schedule, "Registered API tool cron job");
            entries.push(ApiCronEntry { tool: tool.clone(), cron: cron.clone() });
        }
    }
    if entries.is_empty() {
        return;
    }
    let count = entries.len();
    *CRON_ENTRIES.write().await = entries;
    tokio::spawn(cron_loop(home_dir));
    info!(count, "API tool cron scheduler started");
}

async fn cron_loop(home_dir: PathBuf) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    interval.tick().await; // skip immediate fire
    loop {
        interval.tick().await;
        let entries = CRON_ENTRIES.read().await.clone();
        let now = chrono::Local::now();
        for entry in entries {
            if !is_due(&entry.cron.schedule, &now) {
                continue;
            }
            let tool_name = entry.tool.name.clone();
            let tool_config = entry.tool.clone();
            let home = home_dir.clone();
            tokio::spawn(async move {
                info!(tool = %tool_name, "API cron: executing");
                match execute_cron_api_call(&tool_config, &home).await {
                    Ok(_) => info!(tool = %tool_name, "API cron: success"),
                    Err(e) => warn!(tool = %tool_name, error = %e, "API cron: failed"),
                }
            });
        }
    }
}

async fn execute_cron_api_call(tool: &ApiToolDef, home_dir: &std::path::Path) -> Result<(), String> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;

    let url = build_cron_url(tool);
    let mut req = http.get(&url);
    for (k, v) in &tool.headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| format!("Request: {e}"))?;
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("Parse: {e}"))?;

    if let Some(ref check) = tool.error_check {
        let actual = navigate(&body, &check.field)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        if actual != check.expect {
            return Err(format!("API error: {}='{}'", check.field, actual));
        }
    }

    if let Some(save_to) = tool.cron.as_ref().and_then(|c| c.save_to.as_deref()) {
        let db_path = save_to.strip_prefix("sqlite:").unwrap_or(save_to);
        let table = tool.cron.as_ref().and_then(|c| c.table.clone())
            .unwrap_or_else(|| format!("api_cron_{}", tool.name));
        store_to_sqlite(home_dir, db_path, &table, &tool.name, &body)?;
    }
    Ok(())
}

fn build_cron_url(tool: &ApiToolDef) -> String {
    let mut url = tool.url.clone();
    let mut query_parts: Vec<String> = Vec::new();
    for (name, param_def) in &tool.params {
        let placeholder = format!("{{{}}}", name);
        if url.contains(&placeholder) {
            if let Some(ref default) = param_def.default {
                let val_str = json_to_str(default);
                url = url.replace(&placeholder, &urlencoding::encode(&val_str));
            }
        } else if let Some(ref default) = param_def.default {
            query_parts.push(format!("{}={}", urlencoding::encode(name), urlencoding::encode(&json_to_str(default))));
        }
    }
    if let (Some(ref auth_env), Some(ref auth_param)) = (&tool.auth_env, &tool.auth_param) {
        if let Ok(key) = std::env::var(auth_env) {
            if !key.is_empty() {
                query_parts.push(format!("{}={}", urlencoding::encode(auth_param), urlencoding::encode(&key)));
            }
        }
    }
    if query_parts.is_empty() {
        url
    } else if url.contains('?') {
        format!("{}&{}", url, query_parts.join("&"))
    } else {
        format!("{}?{}", url, query_parts.join("&"))
    }
}

fn json_to_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Check if a cron expression is due at the current time.
fn is_due(schedule: &str, now: &chrono::DateTime<chrono::Local>) -> bool {
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    let minute = now.format("%M").to_string().parse::<u32>().unwrap_or(0);
    let hour = now.format("%H").to_string().parse::<u32>().unwrap_or(0);
    let dom = now.format("%d").to_string().parse::<u32>().unwrap_or(0);
    let month = now.format("%m").to_string().parse::<u32>().unwrap_or(0);
    let dow = now.format("%w").to_string().parse::<u32>().unwrap_or(0);
    cron_match(parts[0], minute)
        && cron_match(parts[1], hour)
        && cron_match(parts[2], dom)
        && cron_match(parts[3], month)
        && cron_match(parts[4], dow)
}

fn cron_match(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(n_str) = field.strip_prefix("*/") {
        if let Ok(n) = n_str.parse::<u32>() {
            if n > 0 {
                return value.is_multiple_of(n);
            }
        }
        return false;
    }
    for part in field.split(',') {
        if let Ok(v) = part.trim().parse::<u32>() {
            if v == value {
                return true;
            }
        }
    }
    false
}

fn navigate<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        if let Some(bracket) = segment.find('[') {
            let field = &segment[..bracket];
            let idx_str = &segment[bracket + 1..segment.len() - 1];
            if !field.is_empty() {
                current = current.get(field)?;
            }
            let idx: usize = idx_str.parse().ok()?;
            current = current.get(idx)?;
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current)
}

fn store_to_sqlite(home_dir: &std::path::Path, db_path: &str, table: &str, tool_name: &str, body: &serde_json::Value) -> Result<(), String> {
    let full_path = if db_path.starts_with('/') {
        std::path::PathBuf::from(db_path)
    } else {
        home_dir.join(db_path)
    };
    let conn = rusqlite::Connection::open(&full_path).map_err(|e| format!("SQLite open: {e}"))?;
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {} (id INTEGER PRIMARY KEY AUTOINCREMENT, tool_name TEXT NOT NULL, raw_response TEXT, fetched_at TEXT DEFAULT (datetime('now','localtime')))",
            table
        ),
        [],
    ).map_err(|e| format!("Create table: {e}"))?;
    let raw = serde_json::to_string(body).unwrap_or_default();
    conn.execute(
        &format!("INSERT INTO {} (tool_name, raw_response) VALUES (?1, ?2)", table),
        rusqlite::params![tool_name, raw],
    ).map_err(|e| format!("Insert: {e}"))?;
    Ok(())
}
