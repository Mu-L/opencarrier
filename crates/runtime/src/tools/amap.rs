//! Built-in Amap (Gaode Maps) tools — direct REST API calls.
//!
//! Replaces the old amap-mcp (standalone MCP server) with in-process HTTP
//! calls via WebFetchEngine. Two tools:
//! - amap_geocode: address → coordinates + formatted address
//! - amap_driving: origin/destination → distance, duration, tolls, tier
//!
//! API key is read from the `AMAP_API_KEY` environment variable at call time
//! (same convention as the old MCP server config).

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct AmapTools;

fn api_key() -> Option<String> {
    std::env::var("AMAP_API_KEY").ok().filter(|s| !s.is_empty())
}

/// Check if a string looks like coordinates (contains comma, no CJK chars).
fn is_coordinates(s: &str) -> bool {
    s.contains(',') && !s.chars().any(|c| c > '\u{4e00}' && c < '\u{9fff}')
}

#[async_trait]
impl ToolModule for AmapTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        // If no API key configured, don't register the tools at all.
        if api_key().is_none() {
            return vec![];
        }

        vec![
            ToolDefinition {
                name: "amap_geocode".to_string(),
                description: "将地名地址转换为经纬度坐标。输入地名，返回经纬度和标准地址。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "address": {
                            "type": "string",
                            "description": "地名地址，如'广州天河'、'南京南站'"
                        }
                    },
                    "required": ["address"]
                }),
            },
            ToolDefinition {
                name: "amap_driving".to_string(),
                description: "驾驶路线规划。输入起终点（地名或经纬度），返回里程(km)、预计时间(分钟)、过路费(元)、收费路段距离(km)、距离档位(市内/近郊/远郊/长途)。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "origin": {
                            "type": "string",
                            "description": "起点，地名（如'广州天河'）或经纬度（如'113.36,23.12'）"
                        },
                        "destination": {
                            "type": "string",
                            "description": "终点，地名（如'佛山顺德'）或经纬度（如'113.24,22.80'）"
                        }
                    },
                    "required": ["origin", "destination"]
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
            "amap_geocode" => {
                let address = input["address"].as_str().unwrap_or("");
                if address.is_empty() {
                    return Some(Err("address is required".to_string()));
                }
                Some(do_geocode(address, ctx).await)
            }
            "amap_driving" => {
                let origin = input["origin"].as_str().unwrap_or("");
                let destination = input["destination"].as_str().unwrap_or("");
                if origin.is_empty() || destination.is_empty() {
                    return Some(Err("origin and destination are required".to_string()));
                }
                Some(do_driving(origin, destination, ctx).await)
            }
            _ => None,
        }
    }

    fn permission_level(&self, _tool_name: &str) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }
}

// ------------------------------------------------------------------ //
//  Geocode                                                            //
// ------------------------------------------------------------------ //

async fn do_geocode(address: &str, ctx: &ToolContext<'_>) -> Result<String, String> {
    let key = api_key().ok_or("AMAP_API_KEY not configured")?;
    let engine = ctx.fetch_engine.ok_or("Fetch engine not available")?;

    let url = format!(
        "https://restapi.amap.com/v3/geocode/geo?address={}&key={}",
        urlencoding::encode(address),
        key
    );

    let raw = engine.fetch(&url).await.map_err(|e| format!("Geocode request failed: {e}"))?;

    // Strip "HTTP 200\n\n" prefix and external content wrapper from fetch engine
    let body = extract_body(&raw);

    let resp: GeocodeResponse = serde_json::from_str(body)
        .map_err(|e| format!("Geocode parse error: {e}"))?;

    if resp.status != "1" {
        return Err(format!("Geocode API returned status={}", resp.status));
    }

    let geo = resp.geocodes
        .and_then(|g| g.into_iter().next())
        .ok_or_else(|| format!("No geocode result for address: {}", address))?;

    let location = geo.location.ok_or_else(|| "No location in geocode result".to_string())?;
    let formatted = geo.formatted_address.unwrap_or_default();

    let result = serde_json::json!({
        "location": location,
        "formatted_address": formatted,
    });

    Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
}

// ------------------------------------------------------------------ //
//  Driving                                                            //
// ------------------------------------------------------------------ //

async fn do_driving(origin: &str, destination: &str, ctx: &ToolContext<'_>) -> Result<String, String> {
    let key = api_key().ok_or("AMAP_API_KEY not configured")?;
    let engine = ctx.fetch_engine.ok_or("Fetch engine not available")?;

    // Resolve origin/destination to coordinates if they are place names
    let origin_coords = resolve_location(origin, ctx).await?;
    let dest_coords = resolve_location(destination, ctx).await?;

    let url = format!(
        "https://restapi.amap.com/v3/direction/driving?origin={}&destination={}&key={}&extensions=base",
        urlencoding::encode(&origin_coords),
        urlencoding::encode(&dest_coords),
        key
    );

    let raw = engine.fetch(&url).await.map_err(|e| format!("Driving request failed: {e}"))?;
    let body = extract_body(&raw);

    let resp: DrivingResponse = serde_json::from_str(body)
        .map_err(|e| format!("Driving parse error: {e}"))?;

    if resp.status != "1" {
        return Err(format!("Driving API returned status={}", resp.status));
    }

    let route = resp.route.ok_or_else(|| "No route in response".to_string())?;
    let path = route.paths
        .and_then(|p| p.into_iter().next())
        .ok_or_else(|| "No path in route".to_string())?;

    let distance_m: f64 = path.distance.as_deref().unwrap_or("0").parse().unwrap_or(0.0);
    let duration_s: f64 = path.duration.as_deref().unwrap_or("0").parse().unwrap_or(0.0);
    let tolls: f64 = path.tolls.as_deref().unwrap_or("0").parse().unwrap_or(0.0);
    let toll_distance_m: f64 = path.toll_distance.as_deref().unwrap_or("0").parse().unwrap_or(0.0);
    let taxi_cost: f64 = route.taxi_cost.as_deref().unwrap_or("0").parse().unwrap_or(0.0);

    let distance_km = distance_m / 1000.0;
    let duration_min = duration_s / 60.0;
    let toll_distance_km = toll_distance_m / 1000.0;

    let distance_tier = if distance_km <= 50.0 {
        "市内"
    } else if distance_km <= 150.0 {
        "近郊"
    } else if distance_km <= 300.0 {
        "远郊"
    } else {
        "长途"
    };

    let result = serde_json::json!({
        "distance_km": (distance_km * 10.0).round() / 10.0,
        "duration_min": duration_min.round() as i64,
        "tolls": tolls as i64,
        "toll_distance_km": (toll_distance_km * 10.0).round() / 10.0,
        "taxi_cost": taxi_cost as i64,
        "distance_tier": distance_tier,
    });

    Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
}

/// Resolve a location string to coordinates. If already coordinates, return as-is.
/// Otherwise geocode it.
async fn resolve_location(location: &str, ctx: &ToolContext<'_>) -> Result<String, String> {
    if is_coordinates(location) {
        Ok(location.to_string())
    } else {
        let (coords, _) = do_geocode_raw(location, ctx).await?;
        Ok(coords)
    }
}

/// Low-level geocode returning (location, formatted_address) tuple.
async fn do_geocode_raw(address: &str, ctx: &ToolContext<'_>) -> Result<(String, String), String> {
    let key = api_key().ok_or("AMAP_API_KEY not configured")?;
    let engine = ctx.fetch_engine.ok_or("Fetch engine not available")?;

    let url = format!(
        "https://restapi.amap.com/v3/geocode/geo?address={}&key={}",
        urlencoding::encode(address),
        key
    );

    let raw = engine.fetch(&url).await.map_err(|e| format!("Geocode request failed: {e}"))?;
    let body = extract_body(&raw);

    let resp: GeocodeResponse = serde_json::from_str(body)
        .map_err(|e| format!("Geocode parse error: {e}"))?;

    if resp.status != "1" {
        return Err(format!("Geocode API returned status={}", resp.status));
    }

    let geo = resp.geocodes
        .and_then(|g| g.into_iter().next())
        .ok_or_else(|| format!("No geocode result for address: {}", address))?;

    let location = geo.location.ok_or_else(|| "No location in geocode result".to_string())?;
    let formatted = geo.formatted_address.unwrap_or_default();

    Ok((location, formatted))
}

/// Strip "HTTP 200\n\n" prefix and [external_content] wrapper from WebFetchEngine output.
fn extract_body(raw: &str) -> &str {
    // WebFetchEngine returns "HTTP {status}\n\n{content}"
    let s = raw.strip_prefix("HTTP 200\n\n").unwrap_or(raw);
    // Strip [external_content url]...[/external_content] wrapper if present
    if let Some(start) = s.find("]\n") {
        if s.starts_with("[external_content ") {
            let inner = &s[start + 2..];
            if let Some(end) = inner.rfind("\n[/external_content]") {
                return &inner[..end];
            }
            return inner;
        }
    }
    s
}

// ------------------------------------------------------------------ //
//  API response types                                                 //
// ------------------------------------------------------------------ //

#[derive(Debug, serde::Deserialize)]
struct GeocodeResponse {
    status: String,
    geocodes: Option<Vec<GeocodeEntry>>,
}

#[derive(Debug, serde::Deserialize)]
struct GeocodeEntry {
    formatted_address: Option<String>,
    location: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct DrivingResponse {
    status: String,
    route: Option<DrivingRoute>,
}

#[derive(Debug, serde::Deserialize)]
struct DrivingRoute {
    paths: Option<Vec<DrivingPath>>,
    taxi_cost: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct DrivingPath {
    distance: Option<String>,
    duration: Option<String>,
    tolls: Option<String>,
    toll_distance: Option<String>,
}
