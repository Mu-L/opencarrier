//! Built-in Amap (Gaode Maps) tools — direct REST API calls via reqwest.
//!
//! Replaces the old amap-mcp (standalone MCP server) with in-process HTTP
//! calls. Two tools:
//! - amap_geocode: address → coordinates + formatted address
//! - amap_driving: origin/destination → distance, duration, tolls, tier
//!
//! API key is read from the `AMAP_API_KEY` environment variable at call time.
//! Uses reqwest directly (not WebFetchEngine) because Amap returns raw JSON
//! — no SSRF risk for fixed API host, no HTML conversion needed.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct AmapTools;

fn api_key() -> Option<String> {
    std::env::var("AMAP_API_KEY").ok().filter(|s| !s.is_empty())
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// Check if a string looks like coordinates (contains comma, no CJK chars).
fn is_coordinates(s: &str) -> bool {
    s.contains(',') && !s.chars().any(|c| c > '\u{4e00}' && c < '\u{9fff}')
}

#[async_trait]
impl ToolModule for AmapTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
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
        _ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        match name {
            "amap_geocode" => {
                let address = input["address"].as_str().unwrap_or("");
                if address.is_empty() {
                    return Some(Err("address is required".to_string()));
                }
                Some(do_geocode(address).await)
            }
            "amap_driving" => {
                let origin = input["origin"].as_str().unwrap_or("");
                let destination = input["destination"].as_str().unwrap_or("");
                if origin.is_empty() || destination.is_empty() {
                    return Some(Err("origin and destination are required".to_string()));
                }
                Some(do_driving(origin, destination).await)
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

async fn do_geocode(address: &str) -> Result<String, String> {
    let key = api_key().ok_or("AMAP_API_KEY not configured")?;
    let client = http_client();

    let url = format!(
        "https://restapi.amap.com/v3/geocode/geo?address={}&key={}",
        urlencoding::encode(address),
        key
    );

    let resp: GeocodeResponse = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Geocode request failed: {e}"))?
        .json()
        .await
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

async fn do_driving(origin: &str, destination: &str) -> Result<String, String> {
    let key = api_key().ok_or("AMAP_API_KEY not configured")?;
    let client = http_client();

    // Resolve origin/destination to coordinates if they are place names
    let origin_coords = resolve_location(origin).await?;
    let dest_coords = resolve_location(destination).await?;

    let url = format!(
        "https://restapi.amap.com/v3/direction/driving?origin={}&destination={}&key={}&extensions=base",
        urlencoding::encode(&origin_coords),
        urlencoding::encode(&dest_coords),
        key
    );

    let resp: DrivingResponse = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Driving request failed: {e}"))?
        .json()
        .await
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
async fn resolve_location(location: &str) -> Result<String, String> {
    if is_coordinates(location) {
        Ok(location.to_string())
    } else {
        let (coords, _) = do_geocode_raw(location).await?;
        Ok(coords)
    }
}

/// Low-level geocode returning (location, formatted_address) tuple.
async fn do_geocode_raw(address: &str) -> Result<(String, String), String> {
    let key = api_key().ok_or("AMAP_API_KEY not configured")?;
    let client = http_client();

    let url = format!(
        "https://restapi.amap.com/v3/geocode/geo?address={}&key={}",
        urlencoding::encode(address),
        key
    );

    let resp: GeocodeResponse = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Geocode request failed: {e}"))?
        .json()
        .await
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
