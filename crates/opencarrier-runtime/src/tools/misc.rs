//! Miscellaneous tool module (location, time).

use super::ToolModule;
use async_trait::async_trait;
use crate::tool_context::ToolContext;
use opencarrier_types::tool::ToolDefinition;
use serde_json::Value;

/// Miscellaneous tools (location_get, system_time).
pub struct MiscTools;

#[async_trait]
impl ToolModule for MiscTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "location_get".to_string(),
                description: "Get the current geographical location based on IP address."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "system_time".to_string(),
                description: "Get the current date, time, timezone, and Unix epoch.".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        _input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        match name {
            "location_get" => Some(location_get().await),
            "system_time" => Some(Ok(system_time())),
            _ => None,
        }
    }
}

async fn location_get() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;
    let resp = client
        .get("https://ip-api.com/json/?fields=status,message,country,regionName,city,zip,lat,lon,timezone,isp,query")
        .header("User-Agent", format!("OpenCarrier/{}", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .map_err(|e| format!("Location request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Location API returned {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse location response: {e}"))?;
    if body["status"].as_str() != Some("success") {
        let msg = body["message"].as_str().unwrap_or("Unknown error");
        return Err(format!("Location lookup failed: {msg}"));
    }
    let result = serde_json::json!({
        "lat": body["lat"],
        "lon": body["lon"],
        "city": body["city"],
        "region": body["regionName"],
        "country": body["country"],
        "zip": body["zip"],
        "timezone": body["timezone"],
        "isp": body["isp"],
        "ip": body["query"],
    });
    serde_json::to_string_pretty(&result).map_err(|e| format!("Serialize error: {e}"))
}

fn system_time() -> String {
    let now_utc = chrono::Utc::now();
    let now_local = chrono::Local::now();
    let result = serde_json::json!({
        "utc": now_utc.to_rfc3339(),
        "local": now_local.to_rfc3339(),
        "unix_epoch": now_utc.timestamp(),
        "timezone": now_local.format("%Z").to_string(),
        "utc_offset": now_local.format("%:z").to_string(),
        "date": now_local.format("%Y-%m-%d").to_string(),
        "time": now_local.format("%H:%M:%S").to_string(),
        "day_of_week": now_local.format("%A").to_string(),
    });
    serde_json::to_string_pretty(&result).unwrap_or_else(|_| now_utc.to_rfc3339())
}
