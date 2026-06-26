//! amap-mcp — Amap (Gaode Maps) MCP Server
//!
//! Provides geocoding and driving direction tools. API key is read from
//! the `AMAP_API_KEY` environment variable (configured via config.toml env section).

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, transport::stdio as stdio_transport, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use mcp_common::json::{error_response, json_to_string};

// ================================================================== //
//  API key                                                             //
// ================================================================== //

fn api_key() -> String {
    std::env::var("AMAP_API_KEY").unwrap_or_default()
}

// ================================================================== //
//  HTTP client                                                         //
// ================================================================== //

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

// ================================================================== //
//  Geocoding                                                           //
// ================================================================== //

#[derive(Debug, Deserialize)]
struct GeocodeResponse {
    status: String,
    geocodes: Option<Vec<GeocodeEntry>>,
}

#[derive(Debug, Deserialize)]
struct GeocodeEntry {
    formatted_address: Option<String>,
    location: Option<String>,
}

async fn geocode_inner(address: &str) -> Result<(String, String)> {
    let key = api_key();
    if key.is_empty() {
        anyhow::bail!("AMAP_API_KEY environment variable not set");
    }
    let url = format!(
        "https://restapi.amap.com/v3/geocode/geo?address={}&key={}",
        urlencoding::encode(address),
        key
    );
    let resp: GeocodeResponse = http_client().get(&url).send().await?.json().await?;
    if resp.status != "1" {
        anyhow::bail!("Geocode API returned status={}", resp.status);
    }
    let geo = resp
        .geocodes
        .and_then(|g| g.into_iter().next())
        .ok_or_else(|| anyhow::anyhow!("No geocode result for address: {}", address))?;
    let location = geo.location.ok_or_else(|| anyhow::anyhow!("No location in geocode result"))?;
    let formatted = geo.formatted_address.unwrap_or_default();
    Ok((location, formatted))
}

// ================================================================== //
//  Driving direction                                                   //
// ================================================================== //

#[derive(Debug, Deserialize)]
struct DrivingResponse {
    status: String,
    route: Option<DrivingRoute>,
}

#[derive(Debug, Deserialize)]
struct DrivingRoute {
    paths: Option<Vec<DrivingPath>>,
    taxi_cost: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DrivingPath {
    distance: Option<String>,
    duration: Option<String>,
    tolls: Option<String>,
    toll_distance: Option<String>,
}

async fn driving_inner(origin: &str, destination: &str) -> Result<serde_json::Value> {
    let key = api_key();
    if key.is_empty() {
        anyhow::bail!("AMAP_API_KEY environment variable not set");
    }
    let url = format!(
        "https://restapi.amap.com/v3/direction/driving?origin={}&destination={}&key={}&extensions=base",
        urlencoding::encode(origin),
        urlencoding::encode(destination),
        key
    );
    let resp: DrivingResponse = http_client().get(&url).send().await?.json().await?;
    if resp.status != "1" {
        anyhow::bail!("Driving API returned status={}", resp.status);
    }
    let route = resp.route.ok_or_else(|| anyhow::anyhow!("No route in response"))?;
    let path = route
        .paths
        .and_then(|p| p.into_iter().next())
        .ok_or_else(|| anyhow::anyhow!("No path in route"))?;

    let distance_m: f64 = path
        .distance
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let duration_s: f64 = path
        .duration
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let tolls: f64 = path
        .tolls
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let toll_distance_m: f64 = path
        .toll_distance
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let taxi_cost: f64 = route
        .taxi_cost
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);

    let distance_km = distance_m / 1000.0;
    let duration_min = duration_s / 60.0;
    let toll_distance_km = toll_distance_m / 1000.0;

    // Determine distance tier
    let distance_tier = if distance_km <= 50.0 {
        "市内"
    } else if distance_km <= 150.0 {
        "近郊"
    } else if distance_km <= 300.0 {
        "远郊"
    } else {
        "长途"
    };

    Ok(serde_json::json!({
        "distance_km": (distance_km * 10.0).round() / 10.0,
        "duration_min": duration_min.round() as i64,
        "tolls": tolls as i64,
        "toll_distance_km": (toll_distance_km * 10.0).round() / 10.0,
        "taxi_cost": taxi_cost as i64,
        "distance_tier": distance_tier,
    }))
}

/// Check if a string looks like coordinates (contains comma, no CJK chars).
fn is_coordinates(s: &str) -> bool {
    s.contains(',') && !s.chars().any(|c| c > '\u{4e00}' && c < '\u{9fff}')
}

/// Resolve a location string to coordinates. If already coordinates, return as-is.
/// Otherwise geocode it.
async fn resolve_location(location: &str) -> Result<String> {
    if is_coordinates(location) {
        Ok(location.to_string())
    } else {
        let (coords, _addr) = geocode_inner(location).await?;
        Ok(coords)
    }
}

// ================================================================== //
//  Tool parameter structs                                              //
// ================================================================== //

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct GeocodeParams {
    #[schemars(description = "地名地址，如'广州天河'、'南京南站'")]
    address: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct DrivingParams {
    #[schemars(description = "起点，地名（如'广州天河'）或经纬度（如'113.36,23.12'）")]
    origin: String,
    #[schemars(description = "终点，地名（如'佛山顺德'）或经纬度（如'113.24,22.80'）")]
    destination: String,
}

// ================================================================== //
//  MCP Server                                                          //
// ================================================================== //

#[derive(Clone)]
struct AmapServer;

#[tool_router(server_handler)]
impl AmapServer {
    #[tool(description = "将地名地址转换为经纬度坐标。输入地名，返回经纬度和标准地址。")]
    async fn geocode(&self, Parameters(params): Parameters<GeocodeParams>) -> String {
        match geocode_inner(&params.address).await {
            Ok((location, formatted_address)) => {
                json_to_string(&serde_json::json!({
                    "location": location,
                    "formatted_address": formatted_address,
                }))
            }
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "驾驶路线规划。输入起终点（地名或经纬度），返回里程(km)、预计时间(分钟)、过路费(元)、收费路段距离(km)、距离档位(市内/近郊/远郊/长途)。")]
    async fn driving(&self, Parameters(params): Parameters<DrivingParams>) -> String {
        let origin = match resolve_location(&params.origin).await {
            Ok(o) => o,
            Err(e) => return error_response(&e),
        };
        let destination = match resolve_location(&params.destination).await {
            Ok(d) => d,
            Err(e) => return error_response(&e),
        };
        match driving_inner(&origin, &destination).await {
            Ok(result) => json_to_string(&result),
            Err(e) => error_response(&e),
        }
    }
}

// ================================================================== //
//  Main                                                                //
// ================================================================== //

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AMAP_MCP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let server = AmapServer;

    tracing::info!("amap-mcp starting (stdio)");
    let service = server.serve(stdio_transport()).await?;
    service.waiting().await?;

    Ok(())
}
