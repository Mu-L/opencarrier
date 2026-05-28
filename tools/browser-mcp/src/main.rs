//! browser-mcp — Browser automation MCP Server via Chrome DevTools Protocol.
//!
//! Provides 10 tools for browser automation: navigate, click, type, screenshot,
//! read_page, close, scroll, wait, run_js, back.
//!
//! Supports two backends:
//! - **Obscura**: lightweight Rust headless browser (no rendering, no screenshots)
//! - **Chromium**: full browser with rendering and screenshots (fallback)

use anyhow::Result;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, transport::stdio as stdio_transport, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Backend type
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BackendType {
    Chromium,
    Obscura,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BrowserBackend {
    Auto,
    Obscura,
    Chromium,
}

struct BrowserConfig {
    backend: BrowserBackend,
    headless: bool,
    viewport_width: u32,
    viewport_height: u32,
    max_sessions: usize,
    chromium_path: Option<String>,
    cdp_endpoint: Option<String>,
}

impl BrowserConfig {
    fn from_env() -> Self {
        let backend = match std::env::var("BROWSER_BACKEND").as_deref() {
            Ok("obscura") => BrowserBackend::Obscura,
            Ok("chromium") => BrowserBackend::Chromium,
            _ => BrowserBackend::Auto,
        };
        Self {
            backend,
            headless: std::env::var("BROWSER_HEADLESS")
                .map(|v| v != "false")
                .unwrap_or(true),
            viewport_width: std::env::var("BROWSER_VIEWPORT_WIDTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1280),
            viewport_height: std::env::var("BROWSER_VIEWPORT_HEIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(720),
            max_sessions: std::env::var("BROWSER_MAX_SESSIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
            chromium_path: std::env::var("BROWSER_CHROMIUM_PATH").ok(),
            cdp_endpoint: std::env::var("BROWSER_CDP_ENDPOINT").ok(),
        }
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CDP_CONNECT_TIMEOUT_SECS: u64 = 15;
const CDP_COMMAND_TIMEOUT_SECS: u64 = 30;
const PAGE_LOAD_POLL_INTERVAL_MS: u64 = 200;
const PAGE_LOAD_MAX_POLLS: u32 = 150;
const OBSCURA_BASE_PORT: u16 = 19222;
const OBSCURA_MAX_PORT_ATTEMPTS: u16 = 10;

// ---------------------------------------------------------------------------
// CDP Connection
// ---------------------------------------------------------------------------

struct CdpConnection {
    write: Mutex<
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    >,
    pending: Arc<DashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    session_id: Arc<Mutex<Option<String>>>,
    backend: BackendType,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl CdpConnection {
    async fn connect(ws_url: &str) -> Result<Self, String> {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("CDP WebSocket connect failed: {e}"))?;

        let (write, read) = stream.split();
        let pending: Arc<DashMap<u64, oneshot::Sender<Value>>> = Arc::new(DashMap::new());
        let pending_clone = pending.clone();
        let session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let session_id_clone = session_id.clone();

        // oneshot channel to signal when __init is received (obscura)
        let (init_tx, init_rx) = oneshot::channel::<()>();
        let init_tx = Arc::new(Mutex::new(Some(init_tx)));

        let reader_handle = tokio::spawn(async move {
            let mut read = read;
            while let Some(msg) = read.next().await {
                let Ok(msg) = msg else { continue };
                match msg {
                    Message::Text(text) => {
                        if let Ok(val) = serde_json::from_str::<Value>(&text) {
                            // Obscura sends {"__init":true,"sessionId":"...","pageId":"..."}
                            if val.get("__init").and_then(|v| v.as_bool()) == Some(true) {
                                if let Some(sid) = val.get("sessionId").and_then(|v| v.as_str()) {
                                    *session_id_clone.lock().await = Some(sid.to_string());
                                }
                                // Signal that init is received
                                if let Some(tx) = init_tx.lock().await.take() {
                                    let _ = tx.send(());
                                }
                                continue;
                            }
                            if let Some(id) = val.get("id").and_then(|v| v.as_u64()) {
                                if let Some((_, tx)) = pending_clone.remove(&id) {
                                    let _: Result<(), Value> = tx.send(val);
                                }
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        // Wait briefly for obscura's __init message (Chromium won't send one)
        let backend = if tokio::time::timeout(Duration::from_millis(200), init_rx).await.is_ok() {
            BackendType::Obscura
        } else {
            BackendType::Chromium
        };

        Ok(Self {
            write: Mutex::new(write),
            pending,
            next_id: AtomicU64::new(1),
            session_id,
            backend,
            _reader_handle: reader_handle,
        })
    }

    fn backend_type(&self) -> BackendType {
        self.backend
    }

    async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);

        let mut msg = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });

        // Obscura requires sessionId in every command
        if let Some(sid) = self.session_id.lock().await.as_deref() {
            msg.as_object_mut()
                .unwrap()
                .insert("sessionId".to_string(), Value::String(sid.to_string()));
        }

        let mut write = self.write.lock().await;
        write
            .send(Message::Text(serde_json::to_string(&msg).unwrap().into()))
            .await
            .map_err(|e| format!("CDP send failed: {e}"))?;
        drop(write);

        match tokio::time::timeout(Duration::from_secs(CDP_COMMAND_TIMEOUT_SECS), rx).await {
            Ok(Ok(val)) => {
                if let Some(error) = val.get("error") {
                    Err(format!("CDP error: {error}"))
                } else {
                    Ok(val.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            _ => {
                self.pending.remove(&id);
                Err("CDP command timed out".to_string())
            }
        }
    }

    async fn run_js(&self, expression: &str) -> Result<String, String> {
        let result = self
            .send(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": false,
                }),
            )
            .await?;

        if let Some(exception) = result.get("exceptionDetails") {
            let desc = exception
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown error");
            return Err(desc.to_string());
        }

        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(value)
    }
}

// ---------------------------------------------------------------------------
// Browser Session
// ---------------------------------------------------------------------------

struct BrowserSession {
    process: Option<tokio::process::Child>,
    cdp: CdpConnection,
    last_active: Instant,
}

impl BrowserSession {
    async fn launch(config: &BrowserConfig, agent_id: &str) -> Result<Self, String> {
        // 1. Explicit CDP endpoint takes priority
        if let Some(ref endpoint) = config.cdp_endpoint {
            return Self::connect_cdp(endpoint, agent_id).await;
        }

        // 2. Try obscura (unless backend is forced to chromium)
        if config.backend != BrowserBackend::Chromium {
            if let Some(obscura_path) = find_obscura() {
                match Self::launch_obscura(&obscura_path, agent_id).await {
                    Ok(session) => return Ok(session),
                    Err(e) => {
                        if config.backend == BrowserBackend::Obscura {
                            return Err(format!("Obscura launch failed: {e}"));
                        }
                        warn!("Obscura launch failed: {e}, falling back to Chromium");
                    }
                }
            } else if config.backend == BrowserBackend::Obscura {
                return Err("Obscura not found. Install it to ~/.opencarrier/bin/ or add to PATH.".to_string());
            }
        }

        // 3. Fallback to Chromium
        let chromium = find_chromium(config.chromium_path.as_deref()).ok_or(
            "Neither obscura nor Chromium found. Install obscura or Chromium.".to_string(),
        )?;

        let mut cmd = tokio::process::Command::new(&chromium);
        cmd.arg("--remote-debugging-port=0")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking")
            .arg("--disable-client-side-phishing-detection")
            .arg("--disable-default-apps")
            .arg("--disable-hang-monitor")
            .arg("--disable-popup-blocking")
            .arg("--disable-prompt-on-repost")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--metrics-recording-only")
            .arg("--safebrowsing-disable-auto-update");

        if config.headless {
            cmd.arg("--headless=new");
        }

        cmd.arg(format!(
            "--window-size={},{}",
            config.viewport_width, config.viewport_height
        ))
        .arg(format!("--user-data-dir=/tmp/opencarrier-browser-{agent_id}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

        let process = cmd.spawn().map_err(|e| format!("Failed to launch Chromium: {e}"))?;

        // Wait for DevToolsActivePort file
        let port_file = PathBuf::from(format!("/tmp/opencarrier-browser-{agent_id}/DevToolsActivePort"));
        let ws_url = tokio::time::timeout(Duration::from_secs(CDP_CONNECT_TIMEOUT_SECS), async {
            loop {
                if let Ok(content) = tokio::fs::read_to_string(&port_file).await {
                    if let Some(line) = content.lines().nth(1) {
                        if !line.is_empty() {
                            return Ok(line.to_string());
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .map_err(|_| "Timeout waiting for Chromium DevToolsActivePort".to_string())?
        .map_err(|e: String| e)?;

        let cdp = CdpConnection::connect(&ws_url).await?;
        Ok(Self {
            process: Some(process),
            cdp,
            last_active: Instant::now(),
        })
    }

    async fn launch_obscura(obscura_path: &PathBuf, agent_id: &str) -> Result<Self, String> {
        // Find an available port
        let mut port: u16 = 0;
        for offset in 0..OBSCURA_MAX_PORT_ATTEMPTS {
            let candidate = OBSCURA_BASE_PORT + offset;
            if tokio::net::TcpListener::bind(format!("127.0.0.1:{candidate}"))
                .await
                .is_ok()
            {
                port = candidate;
                break;
            }
        }
        if port == 0 {
            return Err("No available port for obscura".to_string());
        }

        let mut cmd = tokio::process::Command::new(obscura_path);
        cmd.arg("serve")
            .arg("--port")
            .arg(port.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let process = cmd.spawn().map_err(|e| format!("Failed to launch obscura: {e}"))?;

        // Wait for obscura to be ready (poll /json/version)
        let endpoint = format!("http://127.0.0.1:{port}");
        let ready = tokio::time::timeout(Duration::from_secs(CDP_CONNECT_TIMEOUT_SECS), async {
            loop {
                if reqwest::get(format!("{endpoint}/json/version")).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;

        if ready.is_err() {
            return Err("Timeout waiting for obscura to start".to_string());
        }

        info!(agent = %agent_id, port, "Launched obscura as browser backend");
        let mut session = Self::connect_cdp(&endpoint, agent_id).await?;
        session.process = Some(process);
        Ok(session)
    }

    async fn connect_cdp(endpoint: &str, agent_id: &str) -> Result<Self, String> {
        // endpoint is like ws://host:port or http://host:port
        let ws_url = if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
            endpoint.to_string()
        } else {
            // HTTP — fetch /json/list to get a page-level websocket URL.
            // /json/version gives browser-level WS which doesn't support Page.navigate.
            let list_url = format!("{}/json/list", endpoint.trim_end_matches('/'));
            let targets: Vec<Value> = reqwest::get(&list_url)
                .await
                .map_err(|e| format!("Failed to fetch CDP target list: {e}"))?
                .json()
                .await
                .map_err(|e| format!("Invalid CDP target list response: {e}"))?;
            targets
                .iter()
                .find_map(|t| {
                    let is_page = t.get("type").and_then(|v| v.as_str()) == Some("page");
                    let ws = t.get("webSocketDebuggerUrl").and_then(|v| v.as_str());
                    if is_page { ws } else { None }
                })
                .ok_or_else(|| "No page target found in CDP target list".to_string())?
                .to_string()
        };

        info!(agent = %agent_id, ws = %ws_url, "Connecting to CDP endpoint");
        let cdp = CdpConnection::connect(&ws_url).await?;
        Ok(Self {
            process: None,
            cdp,
            last_active: Instant::now(),
        })
    }

    async fn navigate(&mut self, url: &str) -> Result<String, String> {
        self.cdp
            .send(
                "Page.navigate",
                serde_json::json!({ "url": url }),
            )
            .await?;
        self.wait_for_load().await
    }

    async fn click(&mut self, x: f64, y: f64) -> Result<String, String> {
        for (m, px, py) in [("mousePressed", x, y), ("mouseReleased", x, y)] {
            self.cdp
                .send(
                    "Input.dispatchMouseEvent",
                    serde_json::json!({ "type": m, "x": px, "y": py, "button": "left", "clickCount": 1 }),
                )
                .await?;
        }
        Ok("Clicked".to_string())
    }

    async fn r#type(&mut self, text: &str) -> Result<String, String> {
        for ch in text.chars() {
            self.cdp
                .send(
                    "Input.dispatchKeyEvent",
                    serde_json::json!({ "type": "char", "text": ch.to_string() }),
                )
                .await?;
        }
        Ok(format!("Typed {} characters", text.len()))
    }

    async fn screenshot(&mut self) -> Result<String, String> {
        if self.cdp.backend_type() == BackendType::Obscura {
            return Err(
                "Screenshots not supported by Obscura backend. \
                 Set BROWSER_CDP_ENDPOINT to a Chromium CDP endpoint for screenshot support.".to_string()
            );
        }
        let result = self
            .cdp
            .send(
                "Page.captureScreenshot",
                serde_json::json!({ "format": "png" }),
            )
            .await?;
        let data = result
            .get("data")
            .and_then(|v| v.as_str())
            .ok_or("No screenshot data")?;
        Ok(data.to_string()) // base64-encoded PNG
    }

    async fn read_page(&mut self) -> Result<String, String> {
        let html = self
            .cdp
            .run_js("document.documentElement.outerHTML")
            .await?;
        Ok(html)
    }

    async fn scroll(&mut self, direction: &str, amount: u32) -> Result<String, String> {
        let (dx, dy) = match direction {
            "up" => (0, -(amount as i32) * 100),
            "down" => (0, (amount as i32) * 100),
            "left" => (-(amount as i32) * 100, 0),
            "right" => ((amount as i32) * 100, 0),
            _ => return Err(format!("Invalid direction: {direction}")),
        };
        self.cdp
            .run_js(&format!("window.scrollBy({dx},{dy})"))
            .await?;
        Ok(format!("Scrolled {direction} by {amount}"))
    }

    async fn wait(&mut self, seconds: f64) -> Result<String, String> {
        tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
        Ok(format!("Waited {seconds}s"))
    }

    async fn run_js(&mut self, expression: &str) -> Result<String, String> {
        self.cdp.run_js(expression).await
    }

    async fn go_back(&mut self) -> Result<String, String> {
        self.cdp
            .send("Page.navigateBack", serde_json::json!({}))
            .await
            .ok(); // Not all browsers support this
        self.cdp
            .run_js("history.back()")
            .await?;
        self.wait_for_load().await
    }

    async fn wait_for_load(&mut self) -> Result<String, String> {
        for _ in 0..PAGE_LOAD_MAX_POLLS {
            let state = self
                .cdp
                .run_js("document.readyState")
                .await
                .unwrap_or_default();
            if state == "complete" || state == "interactive" {
                return Ok(state);
            }
            tokio::time::sleep(Duration::from_millis(PAGE_LOAD_POLL_INTERVAL_MS)).await;
        }
        Ok("timeout".to_string())
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        if let Some(ref mut process) = self.process {
            let _ = process.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Browser Manager
// ---------------------------------------------------------------------------

struct BrowserManager {
    sessions: DashMap<String, Arc<Mutex<BrowserSession>>>,
    config: Arc<BrowserConfig>,
}

impl BrowserManager {
    fn new(config: BrowserConfig) -> Self {
        Self {
            sessions: DashMap::new(),
            config: Arc::new(config),
        }
    }

    async fn send_command(&self, agent_id: &str, cmd: &str, params: Value) -> Result<String, String> {
        let session = self.get_or_create(agent_id).await?;
        let mut session = session.lock().await;
        session.last_active = Instant::now();

        match cmd {
            "navigate" => {
                let url = params.get("url").and_then(|v| v.as_str()).ok_or("Missing url")?;
                session.navigate(url).await
            }
            "click" => {
                let x = params.get("x").and_then(|v| v.as_f64()).ok_or("Missing x")?;
                let y = params.get("y").and_then(|v| v.as_f64()).ok_or("Missing y")?;
                session.click(x, y).await
            }
            "type" => {
                let text = params.get("text").and_then(|v| v.as_str()).ok_or("Missing text")?;
                session.r#type(text).await
            }
            "screenshot" => session.screenshot().await,
            "read_page" => session.read_page().await,
            "close" => {
                self.sessions.remove(agent_id);
                Ok("Closed".to_string())
            }
            "scroll" => {
                let direction = params.get("direction").and_then(|v| v.as_str()).unwrap_or("down");
                let amount = params.get("amount").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
                session.scroll(direction, amount).await
            }
            "wait" => {
                let seconds = params.get("seconds").and_then(|v| v.as_f64()).unwrap_or(1.0);
                session.wait(seconds).await
            }
            "run_js" => {
                let expression = params.get("expression").and_then(|v| v.as_str()).ok_or("Missing expression")?;
                session.run_js(expression).await
            }
            "back" => session.go_back().await,
            _ => Err(format!("Unknown command: {cmd}")),
        }
    }

    async fn get_or_create(&self, agent_id: &str) -> Result<Arc<Mutex<BrowserSession>>, String> {
        if let Some(entry) = self.sessions.get(agent_id) {
            return Ok(entry.value().clone());
        }

        if self.sessions.len() >= self.config.max_sessions {
            // Evict oldest session
            let mut oldest_key = None;
            let mut oldest_time = Instant::now();
            for entry in self.sessions.iter() {
                if let Ok(sess) = entry.value().try_lock() {
                    if sess.last_active < oldest_time {
                        oldest_time = sess.last_active;
                        oldest_key = Some(entry.key().clone());
                    }
                }
            }
            if let Some(key) = oldest_key {
                self.sessions.remove(&key);
            }
        }

        let session = BrowserSession::launch(&self.config, agent_id).await?;
        let session = Arc::new(Mutex::new(session));
        self.sessions.insert(agent_id.to_string(), session.clone());
        Ok(session)
    }
}

// ---------------------------------------------------------------------------
// Obscura discovery
// ---------------------------------------------------------------------------

fn find_obscura() -> Option<PathBuf> {
    // Check BROWSER_OBSCURA_PATH env var
    if let Ok(path) = std::env::var("BROWSER_OBSCURA_PATH") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Some(p);
        }
    }
    // Check ~/.opencarrier/bin/obscura
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".opencarrier").join("bin").join("obscura");
        if p.exists() {
            return Some(p);
        }
    }
    // Check PATH
    which_obscura()
}

fn which_obscura() -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        let p = PathBuf::from(dir).join("obscura");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Chromium discovery
// ---------------------------------------------------------------------------

fn find_chromium(custom_path: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = custom_path {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    let candidates = chromium_candidates();
    candidates.into_iter().find(|p| p.exists())
}

fn chromium_candidates() -> Vec<PathBuf> {
    vec![
        // Common Linux paths
        PathBuf::from("/usr/bin/chromium-browser"),
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/google-chrome-stable"),
        // Snap
        PathBuf::from("/snap/bin/chromium"),
        // macOS
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        // Windows (MSYS2/Git Bash)
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
    ]
}

// ---------------------------------------------------------------------------
// SSRF check (delegated to mcp_common::ssrf)
// ---------------------------------------------------------------------------

fn check_ssrf(url: &str) -> Result<(), String> {
    mcp_common::ssrf::check_ssrf(url)
}

// ---------------------------------------------------------------------------
// Content wrapping (simplified from runtime's web_content::wrap_external_content)
// ---------------------------------------------------------------------------

fn wrap_external_content(source_url: &str, content: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(source_url.as_bytes()));
    format!(
        "<untrusted-source hash=\"{hash}\">\n{content}\n</untrusted-source>"
    )
}

// ---------------------------------------------------------------------------
// MCP Tool Parameter Structs
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct NavigateParams {
    #[schemars(description = "URL to navigate to")]
    url: String,
}

#[derive(Deserialize, JsonSchema)]
struct ClickParams {
    #[schemars(description = "X coordinate")]
    x: f64,
    #[schemars(description = "Y coordinate")]
    y: f64,
}

#[derive(Deserialize, JsonSchema)]
struct TypeParams {
    #[schemars(description = "Text to type")]
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct ScrollParams {
    #[schemars(description = "Direction: up, down, left, right")]
    direction: Option<String>,
    #[schemars(description = "Amount to scroll (default: 3)")]
    amount: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitParams {
    #[schemars(description = "Seconds to wait (default: 1.0)")]
    seconds: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
struct RunJsParams {
    #[schemars(description = "JavaScript expression to evaluate")]
    expression: String,
}

#[derive(Deserialize, JsonSchema)]
struct ScreenshotParams {}

#[derive(Deserialize, JsonSchema)]
struct ReadPageParams {}

#[derive(Deserialize, JsonSchema)]
struct CloseParams {}

#[derive(Deserialize, JsonSchema)]
struct BackParams {}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BrowserMcpServer {
    manager: Arc<BrowserManager>,
}

#[tool_router(server_handler)]
impl BrowserMcpServer {
    #[tool(description = "Navigate the browser to a URL")]
    async fn browser_navigate(
        &self,
        Parameters(params): Parameters<NavigateParams>,
    ) -> Result<String, String> {
        check_ssrf(&params.url)?;
        let agent_id = "default"; // Single-user MCP
        let result = self
            .manager
            .send_command(agent_id, "navigate", serde_json::json!({ "url": params.url }))
            .await?;
        Ok(format!("Navigated. Page state: {result}"))
    }

    #[tool(description = "Click at coordinates")]
    async fn browser_click(
        &self,
        Parameters(params): Parameters<ClickParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command("default", "click", serde_json::json!({ "x": params.x, "y": params.y }))
            .await
    }

    #[tool(description = "Type text into the focused element")]
    async fn browser_type(
        &self,
        Parameters(params): Parameters<TypeParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command("default", "type", serde_json::json!({ "text": params.text }))
            .await
    }

    #[tool(description = "Take a screenshot. Returns base64-encoded PNG. Not supported by Obscura backend.")]
    async fn browser_screenshot(
        &self,
        Parameters(_params): Parameters<ScreenshotParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command("default", "screenshot", serde_json::json!({}))
            .await
    }

    #[tool(description = "Read the current page content as HTML")]
    async fn browser_read_page(
        &self,
        Parameters(_params): Parameters<ReadPageParams>,
    ) -> Result<String, String> {
        let html = self
            .manager
            .send_command("default", "read_page", serde_json::json!({}))
            .await?;
        Ok(wrap_external_content("browser", &html))
    }

    #[tool(description = "Close the browser session")]
    async fn browser_close(
        &self,
        Parameters(_params): Parameters<CloseParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command("default", "close", serde_json::json!({}))
            .await
    }

    #[tool(description = "Scroll the page")]
    async fn browser_scroll(
        &self,
        Parameters(params): Parameters<ScrollParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command(
                "default",
                "scroll",
                serde_json::json!({
                    "direction": params.direction.unwrap_or_else(|| "down".to_string()),
                    "amount": params.amount.unwrap_or(3),
                }),
            )
            .await
    }

    #[tool(description = "Wait for a number of seconds")]
    async fn browser_wait(
        &self,
        Parameters(params): Parameters<WaitParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command(
                "default",
                "wait",
                serde_json::json!({ "seconds": params.seconds.unwrap_or(1.0) }),
            )
            .await
    }

    #[tool(description = "Execute JavaScript in the browser")]
    async fn browser_run_js(
        &self,
        Parameters(params): Parameters<RunJsParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command("default", "run_js", serde_json::json!({ "expression": params.expression }))
            .await
    }

    #[tool(description = "Go back in browser history")]
    async fn browser_back(
        &self,
        Parameters(_params): Parameters<BackParams>,
    ) -> Result<String, String> {
        self.manager
            .send_command("default", "back", serde_json::json!({}))
            .await
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = BrowserConfig::from_env();
    let server = BrowserMcpServer {
        manager: Arc::new(BrowserManager::new(config)),
    };

    info!("browser-mcp starting");

    let service = server.serve(stdio_transport()).await?;
    service.waiting().await?;
    Ok(())
}
