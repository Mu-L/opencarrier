//! SQLite tools: sqlite_query, sqlite_schema.
//!
//! Allows agents to query embedded SQLite databases (e.g. gaokao admission data).
//! All queries are read-only (SELECT / PRAGMA). DML/DDL is rejected.

use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::ToolDefinition;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Module struct
// ---------------------------------------------------------------------------

pub struct SqliteTools;

// ---------------------------------------------------------------------------
// ToolModule implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl super::ToolModule for SqliteTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "sqlite_query".to_string(),
                description: "Execute a read-only SQL query against an SQLite database in the workspace. Only SELECT and PRAGMA are allowed. Returns results as a markdown table.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "db_path": { "type": "string", "description": "Path to the .db file (relative to workspace). If omitted, the first .db file in the workspace is used." },
                        "sql": { "type": "string", "description": "SQL query to execute. Only SELECT and PRAGMA statements are permitted." }
                    },
                    "required": ["sql"]
                }),
            },
            ToolDefinition {
                name: "sqlite_schema".to_string(),
                description: "List all tables and their columns in an SQLite database.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "db_path": { "type": "string", "description": "Path to the .db file (relative to workspace). If omitted, the first .db file in the workspace is used." }
                    },
                    "required": []
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
        match name {
            "sqlite_query" => Some(tool_sqlite_query(input, ctx.workspace_root).await),
            "sqlite_schema" => Some(tool_sqlite_schema(input, ctx.workspace_root).await),
            _ => None,
        }
    }

    fn permission_level(&self, _tool_name: &str) -> types::tool::PermissionLevel {
        types::tool::PermissionLevel::ReadOnly
    }

    fn max_result_size_chars(&self, _tool_name: &str) -> Option<usize> {
        Some(30_000)
    }
}

// ---------------------------------------------------------------------------
// Private tool implementations
// ---------------------------------------------------------------------------

const MAX_ROWS: usize = 200;
const QUERY_TIMEOUT_SECS: u64 = 30;

/// Resolve db_path: use explicit path, or auto-discover first .db in workspace.
fn resolve_db_path(input: &Value, workspace_root: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = input["db_path"].as_str() {
        let resolved = super::resolve_file_path(path, workspace_root)?;
        if !resolved.exists() {
            return Err(format!("Database not found: {}", resolved.display()));
        }
        if !resolved.extension().map(|e| e == "db").unwrap_or(false) {
            return Err(format!("File must have .db extension: {}", resolved.display()));
        }
        return Ok(resolved);
    }

    // Auto-discover first .db file in workspace
    if let Some(root) = workspace_root {
        let mut found = None;
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "db").unwrap_or(false) {
                    found = Some(path);
                    break;
                }
            }
        }
        // Also check data/ subdirectory
        if found.is_none() {
            let data_dir = root.join("data");
            if let Ok(entries) = std::fs::read_dir(&data_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "db").unwrap_or(false) {
                        found = Some(path);
                        break;
                    }
                }
            }
        }
        if let Some(path) = found {
            return Ok(path);
        }
    }

    Err("No database file found. Provide db_path or place a .db file in the workspace.".to_string())
}

/// Validate that SQL is read-only (SELECT or PRAGMA).
fn validate_readonly_sql(sql: &str) -> Result<(), String> {
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();

    // Reject obvious DML/DDL
    let forbidden = [
        "INSERT ", "UPDATE ", "DELETE ", "DROP ", "CREATE ", "ALTER ",
        "REPLACE ", "TRUNCATE ", "ATTACH ", "DETACH ", "PRAGMA WRITABLE",
    ];
    for kw in &forbidden {
        if upper.contains(kw) {
            return Err(format!(
                "Only read-only queries (SELECT / PRAGMA) are allowed. Forbidden keyword detected: {}",
                kw.trim()
            ));
        }
    }

    // Must start with SELECT or PRAGMA
    if !upper.starts_with("SELECT") && !upper.starts_with("PRAGMA") && !upper.starts_with("WITH") {
        return Err("Query must start with SELECT, WITH, or PRAGMA".to_string());
    }

    Ok(())
}

/// Execute a SELECT/PRAGMA query and return markdown table.
async fn tool_sqlite_query(
    input: &Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let sql = input["sql"].as_str().ok_or("Missing 'sql' parameter")?;
    validate_readonly_sql(sql)?;

    let db_path = resolve_db_path(input, workspace_root)?;

    // Run in blocking thread because rusqlite is sync
    let sql = sql.to_string();
    let db_path_str = db_path.display().to_string();

    let inner = tokio::time::timeout(
        Duration::from_secs(QUERY_TIMEOUT_SECS),
        tokio::task::spawn_blocking(move || {
            run_query(&db_path_str, &sql)
        }),
    )
    .await
    .map_err(|_| "Query timed out".to_string())?
    .map_err(|e| format!("Task failed: {e}"))?;
    inner
}

fn run_query(db_path: &str, sql: &str) -> Result<String, String> {
    use rusqlite::{Connection, types::ValueRef};

    let conn = Connection::open(db_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn.prepare(sql)
        .map_err(|e| format!("Failed to prepare query: {e}"))?;

    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();

    let mut rows = stmt.query([])
        .map_err(|e| format!("Failed to execute query: {e}"))?;

    let mut result_rows: Vec<Vec<String>> = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let mut cols = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    let val = row.get_ref(i).unwrap_or(ValueRef::Null);
                    let s = match val {
                        ValueRef::Null => "NULL".to_string(),
                        ValueRef::Integer(n) => n.to_string(),
                        ValueRef::Real(f) => format!("{:.2}", f),
                        ValueRef::Text(t) => String::from_utf8_lossy(t).to_string(),
                        ValueRef::Blob(b) => format!("<BLOB {} bytes>", b.len()),
                    };
                    cols.push(s);
                }
                result_rows.push(cols);
                if result_rows.len() >= MAX_ROWS {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("Row read error: {e}")),
        }
    }

    let total_fetched = result_rows.len();
    let truncated = total_fetched >= MAX_ROWS;

    // Build markdown table
    let mut md = String::new();
    md.push_str("| ");
    md.push_str(&col_names.join(" | "));
    md.push_str(" |\n");
    md.push_str("| ");
    md.push_str(&col_names.iter().map(|_| "---".to_string()).collect::<Vec<_>>().join(" | "));
    md.push_str(" |\n");

    for cols in &result_rows {
        md.push_str("| ");
        let escaped: Vec<String> = cols.iter().map(|c| {
            // Escape pipe chars in cell content
            c.replace('|', "\\|").replace('\n', " ").replace('\r', "")
        }).collect();
        md.push_str(&escaped.join(" | "));
        md.push_str(" |\n");
    }

    if truncated {
        md.push_str(&format!("\n*[Results limited to {} rows. Use WHERE/LIMIT to narrow.]*", MAX_ROWS));
    } else {
        md.push_str(&format!("\n*{total_fetched} rows returned.*"));
    }

    Ok(md)
}

/// Show schema of all tables.
async fn tool_sqlite_schema(
    input: &Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let db_path = resolve_db_path(input, workspace_root)?;
    let db_path_str = db_path.display().to_string();

    let inner = tokio::time::timeout(
        Duration::from_secs(QUERY_TIMEOUT_SECS),
        tokio::task::spawn_blocking(move || {
            run_schema(&db_path_str)
        }),
    )
    .await
    .map_err(|_| "Schema query timed out".to_string())?
    .map_err(|e| format!("Task failed: {e}"))?;
    inner
}

fn run_schema(db_path: &str) -> Result<String, String> {
    use rusqlite::Connection;

    let conn = Connection::open(db_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;

    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    ).map_err(|e| format!("Failed to list tables: {e}"))?;

    let table_iter = stmt.query_map([], |row| {
        row.get::<_, String>(0)
    }).map_err(|e| format!("Query failed: {e}"))?;

    let mut table_names = Vec::new();
    for name_result in table_iter {
        match name_result {
            Ok(name) => table_names.push(name),
            Err(e) => return Err(format!("Row error: {e}")),
        }
    }

    let mut output = format!("Database: `{db_path}`\n\n**Tables:** {}\n\n", table_names.len());

    for table in &table_names {
        output.push_str(&format!("## Table: `{table}`\n\n"));

        let pragma = format!("PRAGMA table_info({})", table);
        let mut stmt = conn.prepare(&pragma)
            .map_err(|e| format!("Failed to get schema for {table}: {e}"))?;

        let mut rows = stmt.query([])
            .map_err(|e| format!("Query failed: {e}"))?;

        let mut columns: Vec<(String, String, String, String)> = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    let name: String = row.get(1).unwrap_or_default();
                    let ty: String = row.get(2).unwrap_or_default();
                    let notnull: i32 = row.get(3).unwrap_or(0);
                    let dflt: Option<String> = row.get(4).ok();
                    let pk: i32 = row.get(5).unwrap_or(0);

                    let null_str = if notnull == 1 { "NOT NULL" } else { "NULL" };
                    let pk_str = if pk == 1 { "PK" } else { "" };
                    let dflt_str = dflt.map(|d| format!("DEFAULT {d}")).unwrap_or_default();
                    let extra = vec![null_str.to_string(), pk_str.to_string(), dflt_str]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");

                    columns.push((name, ty, extra, if pk == 1 { "✓" } else { "" }.to_string()));
                }
                Ok(None) => break,
                Err(e) => return Err(format!("Row error: {e}")),
            }
        }

        if columns.is_empty() {
            output.push_str("(no columns found)\n\n");
            continue;
        }

        output.push_str("| Column | Type | Constraints | PK |\n");
        output.push_str("|--------|------|-------------|----|\n");
        for (name, ty, extra, pk) in columns {
            output.push_str(&format!("| {name} | {ty} | {extra} | {pk} |\n"));
        }
        output.push('\n');
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sqlite_query_end_to_end() {
        // Create a temp db in the current dir (tests run from workspace root)
        let db_path = std::path::PathBuf::from("test-tmp-sqlite.db");
        let _ = std::fs::remove_file(&db_path);
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
            conn.execute("INSERT INTO t (name) VALUES ('Alice'), ('Bob')", []).unwrap();
        }

        let input = serde_json::json!({
            "db_path": db_path.to_str().unwrap(),
            "sql": "SELECT * FROM t ORDER BY id"
        });
        let result = tool_sqlite_query(&input, None).await;
        assert!(result.is_ok(), "Query failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("Alice"), "Missing Alice: {}", output);
        assert!(output.contains("Bob"), "Missing Bob: {}", output);

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn test_sqlite_schema_end_to_end() {
        let db_path = std::path::PathBuf::from("test-tmp-schema.db");
        let _ = std::fs::remove_file(&db_path);
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL)", []).unwrap();
            conn.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)", []).unwrap();
        }

        let input = serde_json::json!({"db_path": db_path.to_str().unwrap()});
        let result = tool_sqlite_schema(&input, None).await;
        assert!(result.is_ok(), "Schema query failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("users"), "Missing users table: {}", output);
        assert!(output.contains("posts"), "Missing posts table: {}", output);
        assert!(output.contains("email"), "Missing email column: {}", output);

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_validate_readonly_sql_accepts_select() {
        assert!(validate_readonly_sql("SELECT * FROM t").is_ok());
        assert!(validate_readonly_sql("WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
        assert!(validate_readonly_sql("PRAGMA table_info(t)").is_ok());
    }

    #[test]
    fn test_validate_readonly_sql_rejects_dml() {
        assert!(validate_readonly_sql("INSERT INTO t VALUES (1)").is_err());
        assert!(validate_readonly_sql("UPDATE t SET x=1").is_err());
        assert!(validate_readonly_sql("DELETE FROM t").is_err());
        assert!(validate_readonly_sql("DROP TABLE t").is_err());
    }
}
