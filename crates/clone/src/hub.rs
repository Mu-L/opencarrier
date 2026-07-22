//! Hub client — unified interface for all Hub API calls.
//!
//! All Hub HTTP requests go through this module. External callers should
//! never use raw reqwest to call Hub endpoints.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

// === URL validation ===

/// SECURITY: Validate that a Hub URL is safe to fetch (not an internal/metadata endpoint).
/// Uses the shared types::ssrf module for comprehensive SSRF protection.
pub fn validate_hub_url(url: &str) -> Result<()> {
    types::ssrf::check_ssrf(url).map_err(|e| anyhow::anyhow!(e))
}

// === Auth helpers ===

/// Read API key from the configured env var. Falls back to reading ~/.opencarrier/.env directly.
pub fn read_api_key(env_var: &str) -> Result<String> {
    if let Ok(v) = std::env::var(env_var) {
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }
    {
        let env_path = types::config::home_dir().join(".env");
        if let Ok(content) = std::fs::read_to_string(&env_path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if let Some(value) = trimmed.strip_prefix(&format!("{}=", env_var)) {
                    let value = value.trim().to_string();
                    if !value.is_empty() {
                        return Ok(value);
                    }
                }
            }
        }
    }
    anyhow::bail!(
        "API Key 未设置。请设置环境变量 {} (从 Hub 的 Keys 页面获取)",
        env_var
    )
}

#[derive(Deserialize)]
struct AuthResponse {
    token: String,
}

#[derive(Deserialize)]
struct KeyResponse {
    key: String,
}

/// Register a new account on Hub. Returns JWT token.
pub async fn register(
    hub_url: &str,
    username: &str,
    email: &str,
    password: &str,
) -> Result<String> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let resp = reqwest::Client::new()
        .post(format!("{}/api/auth/register", base))
        .json(&serde_json::json!({
            "username": username,
            "email": email,
            "password": password,
        }))
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("注册失败 ({}): {}", status, body);
    }

    let auth: AuthResponse = resp.json().await.context("解析注册响应失败")?;
    Ok(auth.token)
}

/// Login to Hub. Returns JWT token.
pub async fn login(hub_url: &str, login: &str, password: &str) -> Result<String> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let resp = reqwest::Client::new()
        .post(format!("{}/api/auth/login", base))
        .json(&serde_json::json!({
            "login": login,
            "password": password,
        }))
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("登录失败 ({}): {}", status, body);
    }

    let auth: AuthResponse = resp.json().await.context("解析登录响应失败")?;
    Ok(auth.token)
}

/// Create an API key on Hub using JWT token. Returns the plain key.
pub async fn create_api_key(hub_url: &str, jwt: &str, name: &str) -> Result<String> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let resp = reqwest::Client::new()
        .post(format!("{}/api/auth/keys", base))
        .bearer_auth(jwt)
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("创建 API Key 失败: {}", body);
    }

    let data: KeyResponse = resp.json().await.context("解析响应失败")?;
    Ok(data.key)
}

// === Templates ===

#[derive(Deserialize)]
struct SearchResponse {
    templates: Vec<TemplateItem>,
    total: usize,
}

#[derive(Deserialize)]
struct TemplateItem {
    name: String,
    description: String,
    #[allow(dead_code)]
    author: String,
    latest_version: String,
    download_count: i64,
    rating_avg: f64,
}

/// Search templates on Hub. Returns formatted table string.
pub async fn search_templates(hub_url: &str, api_key: &str, query: &str) -> Result<String> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let url = if query.is_empty() {
        format!("{}/api/templates?limit=20", base)
    } else {
        format!(
            "{}/api/templates?q={}&limit=20",
            base,
            urlencoding::encode(query)
        )
    };

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        bail!("Hub 返回错误: {}", resp.status());
    }

    let data: SearchResponse = resp.json().await.context("解析 Hub 响应失败")?;

    if data.templates.is_empty() {
        return Ok("没有找到匹配的模版".to_string());
    }

    let mut out = format!("找到 {} 个模版:\n\n", data.total);
    out.push_str(&format!(
        "{:<25} {:<12} {:<8} {:<6} {}\n",
        "名称", "版本", "下载", "评分", "描述"
    ));
    out.push_str(&format!("{}\n", "-".repeat(80)));

    for t in &data.templates {
        let desc = if t.description.chars().count() > 30 {
            format!("{}…", t.description.chars().take(30).collect::<String>())
        } else {
            t.description.clone()
        };
        let stars = format_stars(t.rating_avg);
        out.push_str(&format!(
            "{:<25} {:<12} {:<8} {:<6} {}\n",
            t.name, t.latest_version, t.download_count, stars, desc
        ));
    }

    Ok(out)
}

/// Search templates on Hub. Returns the raw JSON response.
pub async fn search_templates_json(
    hub_url: &str,
    api_key: &str,
    query: Option<&str>,
    limit: Option<u32>,
) -> Result<serde_json::Value> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let limit = limit.unwrap_or(50);
    let url = if let Some(q) = query {
        format!(
            "{}/api/templates?q={}&limit={}",
            base,
            urlencoding::encode(q),
            limit
        )
    } else {
        format!("{}/api/templates?limit={}", base, limit)
    };

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        bail!("Hub 返回错误: {}", resp.status());
    }

    resp.json().await.context("解析 Hub 响应失败")
}

/// Get a single template's detail from Hub.
pub async fn get_template(
    hub_url: &str,
    api_key: &str,
    name: &str,
) -> Result<serde_json::Value> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let url = format!(
        "{}/api/templates/{}",
        base,
        urlencoding::encode(name)
    );

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        bail!("Hub 返回错误: {}", resp.status());
    }

    resp.json().await.context("解析 Hub 响应失败")
}

/// Hub template key used for download/upgrade (name or hub_template_id).
/// DupHub version endpoints accept the template **name**; we also store that
/// as `hub_template_id` so upgrade does not require a separate UUID.
pub fn hub_template_key(name: &str, hub_template_id: Option<&str>) -> String {
    hub_template_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(name)
        .to_string()
}

/// Download and install a template from Hub.
/// Returns the clone name on success.
///
/// Writes `clone_source.hub_template_id` = Hub template name so later
/// `upgrade` can re-fetch without re-install.
pub async fn install_template(
    hub_url: &str,
    api_key: &str,
    name: &str,
    version: Option<&str>,
    workspace_dir: &Path,
) -> Result<String> {
    tracing::info!("正在从 Hub 下载 {} ...", name);
    let bytes = download_template_bytes(hub_url, api_key, name, version).await?;
    tracing::info!("已下载 {} 字节", bytes.len());

    // v3: extract .agx directly to workspace
    crate::extract_agx(&bytes, workspace_dir)?;

    // hub_template_id = template name (name-based Hub API)
    let hub_id = hub_template_key(name, None);
    let manifest =
        crate::build_manifest_from_workspace(workspace_dir, name, Some(hub_id))?;
    let toml_str = toml::to_string_pretty(&manifest).context("Failed to serialize agent.toml")?;
    std::fs::write(workspace_dir.join("agent.toml"), toml_str)?;

    tracing::info!("分身 '{}' 安装完成 (hub_template_id set for upgrade)", name);
    Ok(name.to_string())
}

/// Download template .agx bytes from Hub (without installing).
/// Used by API routes and upgrade.
pub async fn download_template_bytes(
    hub_url: &str,
    api_key: &str,
    name: &str,
    version: Option<&str>,
) -> Result<Vec<u8>> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let key = urlencoding::encode(name);
    let url = if let Some(v) = version {
        format!(
            "{}/api/templates/{}/versions/{}",
            base,
            key,
            urlencoding::encode(v)
        )
    } else {
        format!("{}/api/templates/{}/versions/latest", base, key)
    };

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("下载失败 {}: {} — {}", name, status, body);
    }

    let bytes = resp.bytes().await.context("读取响应失败")?;
    Ok(bytes.to_vec())
}

/// Fetch a template's file-level manifest from Hub (dup file-level install).
///
/// Returns the same `{files, hash}` shape opencarrier uses locally, so the
/// hash is directly comparable to a local `build_manifest`. `version=None`
/// resolves to latest on the Hub side.
pub async fn fetch_dup_manifest(
    hub_url: &str,
    api_key: &str,
    name: &str,
    version: Option<&str>,
) -> Result<crate::manifest::Manifest> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let key = urlencoding::encode(name);
    let mut url = format!("{}/api/templates/{}/dup/manifest", base, key);
    if let Some(v) = version {
        url.push_str("?version=");
        url.push_str(&urlencoding::encode(v));
    }
    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("拉取 manifest 失败 {}: {} - {}", name, status, body);
    }
    let manifest: crate::manifest::Manifest =
        resp.json().await.context("解析 manifest 失败")?;
    Ok(manifest)
}

/// Fetch a single definition-layer file's raw bytes from Hub (dup file-level
/// install). `path` is relative to the workspace root (e.g. `knowledge/x.md`).
pub async fn fetch_dup_file(
    hub_url: &str,
    api_key: &str,
    name: &str,
    path: &str,
    version: Option<&str>,
) -> Result<Vec<u8>> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let key = urlencoding::encode(name);
    // Encode each segment but keep `/` so the Hub's catch-all route matches.
    let encoded_path = path
        .split('/')
        .map(urlencoding::encode)
        .collect::<Vec<_>>()
        .join("/");
    let mut url = format!("{}/api/templates/{}/dup/file/{}", base, key, encoded_path);
    if let Some(v) = version {
        url.push_str("?version=");
        url.push_str(&urlencoding::encode(v));
    }
    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("拉取文件失败 {} [{}]: {} - {}", name, path, status, body);
    }
    let bytes = resp.bytes().await.context("读取响应失败")?;
    Ok(bytes.to_vec())
}

/// Fetch the manifest + every file for a template via the dup file-level
/// endpoints. Convenience for the install path (replaces downloading one .agx
/// blob). Returns `(manifest, path -> bytes)`.
pub async fn fetch_dup_files(
    hub_url: &str,
    api_key: &str,
    name: &str,
    version: Option<&str>,
) -> Result<(
    crate::manifest::Manifest,
    std::collections::BTreeMap<String, Vec<u8>>,
)> {
    let manifest = fetch_dup_manifest(hub_url, api_key, name, version).await?;
    let mut files: std::collections::BTreeMap<String, Vec<u8>> =
        std::collections::BTreeMap::new();
    for path in manifest.files.keys() {
        let bytes = fetch_dup_file(hub_url, api_key, name, path, version).await?;
        files.insert(path.clone(), bytes);
    }
    Ok((manifest, files))
}

/// Definition-layer files replaced on upgrade (identity / config).
const UPGRADE_TEMPLATE_FILES: &[&str] = &[
    "SOUL.md",
    "system_prompt.md",
    "profile.md",
    "EVOLUTION.md",
    "MEMORY.md",
    "template.json",
    "api_tools.toml",
    "content.toml",
];

/// Definition-layer directories replaced wholesale on upgrade.
const UPGRADE_TEMPLATE_DIRS: &[&str] = &["flows", "agents", "knowledge", "style"];

/// Apply .agx definition files onto an existing workspace (preserve runtime dirs).
///
/// Replaces identity files + flows/agents/knowledge/style; does **not** touch
/// sessions/, senders/, output/, logs/, history/, data/, users/.
///
/// Returns remote version string from staging `template.json` (may be empty).
pub fn apply_agx_definition_upgrade(agx_bytes: &[u8], workspace: &Path) -> Result<String> {
    let staging_dir =
        std::env::temp_dir().join(format!("carrier-upgrade-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        crate::extract_agx(agx_bytes, &staging_dir).context("extract .agx to staging")?;

        let remote_version: String = std::fs::read_to_string(staging_dir.join("template.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<crate::TemplateManifest>(&s).ok())
            .map(|t| t.version)
            .unwrap_or_default();

        for filename in UPGRADE_TEMPLATE_FILES {
            let src = staging_dir.join(filename);
            if src.exists() {
                std::fs::copy(&src, workspace.join(filename))
                    .with_context(|| format!("copy {filename}"))?;
            }
        }

        for dir_name in UPGRADE_TEMPLATE_DIRS {
            let workspace_subdir = workspace.join(dir_name);
            let staging_subdir = staging_dir.join(dir_name);
            if workspace_subdir.exists() {
                let _ = std::fs::remove_dir_all(&workspace_subdir);
            }
            if staging_subdir.exists() {
                copy_dir_recursive(&staging_subdir, &workspace_subdir)
                    .with_context(|| format!("copy dir {dir_name}"))?;
            }
        }

        Ok(remote_version)
    })();

    let _ = std::fs::remove_dir_all(&staging_dir);
    result
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Download from Hub and apply definition-layer upgrade into `workspace`.
///
/// `template_name` is the Hub template name (usually same as agent name).
/// Returns the remote version string from template.json.
pub async fn upgrade_workspace_from_hub(
    hub_url: &str,
    api_key: &str,
    template_name: &str,
    version: Option<&str>,
    workspace: &Path,
    hub_template_id: Option<&str>,
) -> Result<String> {
    let key = hub_template_key(template_name, hub_template_id);
    tracing::info!(
        template = %key,
        version = ?version,
        workspace = %workspace.display(),
        "Upgrading clone definition from Hub"
    );
    let bytes = download_template_bytes(hub_url, api_key, &key, version).await?;
    let remote_version = apply_agx_definition_upgrade(&bytes, workspace)?;

    // Rebuild agent.toml with hub_template_id preserved/set
    let mut manifest = crate::build_manifest_from_workspace(
        workspace,
        template_name,
        Some(key.clone()),
    )?;
    if let Some(ref mut cs) = manifest.clone_source {
        if !remote_version.is_empty() {
            cs.agx_version = remote_version.clone();
        }
        cs.hub_template_id = Some(key);
    }
    let toml_str = toml::to_string_pretty(&manifest).context("serialize agent.toml")?;
    std::fs::write(workspace.join("agent.toml"), toml_str)?;

    Ok(if remote_version.is_empty() {
        "upgraded".to_string()
    } else {
        remote_version
    })
}

/// Publish (upload) a clone .agx to Hub.
pub async fn publish_template(
    hub_url: &str,
    api_key: &str,
    agx_bytes: &[u8],
    category: Option<&str>,
    visibility: Option<&str>,
) -> Result<String> {
    validate_hub_url(hub_url)?;
    use base64::Engine;
    let base = hub_url.trim_end_matches('/');
    let url = format!("{}/api/templates", base);

    let file_base64 = base64::engine::general_purpose::STANDARD.encode(agx_bytes);

    let mut payload = serde_json::json!({
        "file_base64": file_base64,
    });
    if let Some(cat) = category {
        payload["category"] = serde_json::Value::String(cat.to_string());
    }
    if let Some(vis) = visibility {
        payload["visibility"] = serde_json::Value::String(vis.to_string());
    }

    tracing::info!(
        "正在发布到 Hub ({} bytes / {:.1} KB)...",
        agx_bytes.len(),
        agx_bytes.len() as f64 / 1024.0
    );

    let resp = hub_post(&url, api_key)
        .json(&payload)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("发布失败: {} — {}", status, body);
    }

    let body: serde_json::Value = resp.json().await.context("解析 Hub 响应失败")?;
    let name = body["name"].as_str().unwrap_or("unknown");
    let version = body["version"].as_str().unwrap_or("unknown");
    let status = body["status"].as_str().unwrap_or("unknown");
    tracing::info!("发布成功: {} v{} ({})", name, version, status);
    Ok(name.to_string())
}

// === Plugins ===

/// Search plugins on Hub. Returns the raw JSON value.
pub async fn search_plugins(
    hub_url: &str,
    api_key: &str,
    query: &str,
) -> Result<serde_json::Value> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let url = if query.is_empty() {
        format!("{}/api/plugins?limit=20", base)
    } else {
        format!(
            "{}/api/plugins?q={}&limit=20",
            base,
            urlencoding::encode(query)
        )
    };

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        bail!("Hub 返回错误: {}", resp.status());
    }

    resp.json().await.context("解析 Hub 响应失败")
}

/// Detect the current platform string for plugin downloads.
pub fn current_platform() -> String {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    };
    format!("{os}-{arch}")
}

/// Download and install a pre-compiled plugin from Hub.
pub async fn install_plugin(
    hub_url: &str,
    api_key: &str,
    name: &str,
    version: Option<&str>,
    plugins_dir: &Path,
) -> Result<String> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let platform = current_platform();
    let url = if let Some(v) = version {
        format!(
            "{}/api/plugins/{}/versions/{}?platform={}",
            base,
            urlencoding::encode(name),
            urlencoding::encode(v),
            platform
        )
    } else {
        format!(
            "{}/api/plugins/{}/versions/latest?platform={}",
            base,
            urlencoding::encode(name),
            platform
        )
    };

    tracing::info!("正在从 Hub 下载插件 {} (platform={})...", name, platform);

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("下载插件失败 {}: {} — {}", name, status, body);
    }

    let bytes = resp.bytes().await.context("读取响应失败")?;
    tracing::info!("已下载插件 {} 字节", bytes.len());

    let plugin_dir = plugins_dir.join(name);
    std::fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("创建插件目录失败: {}", plugin_dir.display()))?;

    let cursor = std::io::Cursor::new(&bytes[..]);
    let gz = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().with_context(|| "读取插件归档条目失败")? {
        let mut entry = entry.with_context(|| "读取归档条目失败")?;
        let path = entry.path().with_context(|| "获取归档条目路径失败")?;
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("插件归档包含不安全路径: {} (不允许使用 ..)", path.display());
        }
        let path_owned = path.to_path_buf();
        entry
            .unpack_in(&plugin_dir)
            .with_context(|| format!("解压归档条目失败: {}", path_owned.display()))?;
    }

    tracing::info!("插件 '{}' 安装完成 → {}", name, plugin_dir.display());
    Ok(name.to_string())
}

/// Check if a plugin is already installed in the plugins directory.
pub fn is_plugin_installed(plugins_dir: &Path, name: &str) -> bool {
    let plugin_dir = plugins_dir.join(name);
    if !plugin_dir.is_dir() {
        return false;
    }
    if !plugin_dir.join("plugin.toml").exists() {
        return false;
    }
    if let Ok(entries) = std::fs::read_dir(&plugin_dir) {
        for entry in entries.flatten() {
            if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                if ["so", "dylib", "dll"].contains(&ext) {
                    return true;
                }
            }
        }
    }
    false
}

// === MCP Servers ===

/// Download an MCP server manifest from Hub.
pub async fn download_mcp_server(
    hub_url: &str,
    api_key: &str,
    name: &str,
) -> Result<serde_json::Value> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let url = format!(
        "{}/api/mcp-servers/{}/versions/latest",
        base,
        urlencoding::encode(name)
    );

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("下载 MCP Server 失败 {}: {} — {}", name, status, body);
    }

    resp.json().await.context("解析 MCP Server 响应失败")
}

// === Brain Config ===

/// Fetch brain configuration from Hub.
pub async fn fetch_brain_config(
    hub_url: &str,
    api_key: &str,
) -> Result<serde_json::Value> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let url = format!("{}/api/brain/config", base);

    let resp = hub_get(&url, api_key)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("获取 Brain 配置失败: {} — {}", status, body);
    }

    resp.json().await.context("解析 Brain 配置失败")
}

// === Feedback ===

/// Push feedback to Hub.
pub async fn push_feedback(
    hub_url: &str,
    api_key: &str,
    template_name: &str,
    title: &str,
    content: &str,
    source_template: Option<&str>,
) -> Result<()> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let url = format!("{}/api/feedback", base);

    let mut payload = serde_json::json!({
        "template_name": template_name,
        "title": title,
        "content": content,
    });
    if let Some(src) = source_template {
        payload["source_template"] = serde_json::Value::String(src.to_string());
    }

    let resp = hub_post(&url, api_key)
        .json(&payload)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("推送反馈失败: {} — {}", status, body);
    }

    Ok(())
}

// === Releases ===

/// Check for updates on Hub. Returns the latest version string if newer than current.
pub async fn check_update(hub_url: &str, current_version: &str) -> Result<Option<String>> {
    validate_hub_url(hub_url)?;
    let base = hub_url.trim_end_matches('/');
    let url = format!("{}/api/releases", base);

    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("无法连接 Hub")?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let data: serde_json::Value = resp.json().await.ok().unwrap_or_default();
    let latest = match data["latest"].as_str() {
        Some(v) => v,
        None => return Ok(None),
    };

    if latest != current_version && !latest.is_empty() {
        Ok(Some(latest.to_string()))
    } else {
        Ok(None)
    }
}

// === Internal helpers ===

fn hub_request(method: reqwest::Method, url: &str, api_key: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .request(method, url)
        .bearer_auth(api_key)
}

fn hub_get(url: &str, api_key: &str) -> reqwest::RequestBuilder {
    hub_request(reqwest::Method::GET, url, api_key)
}

fn hub_post(url: &str, api_key: &str) -> reqwest::RequestBuilder {
    hub_request(reqwest::Method::POST, url, api_key)
}

fn format_stars(avg: f64) -> String {
    let full = (avg / 1.0).round() as i32;
    (0..5)
        .map(|i| if i < full { "★" } else { "☆" })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_template_key_prefers_id() {
        assert_eq!(hub_template_key("ai-writer", Some("uuid-1")), "uuid-1");
        assert_eq!(hub_template_key("ai-writer", Some("  ")), "ai-writer");
        assert_eq!(hub_template_key("ai-writer", None), "ai-writer");
    }
}
