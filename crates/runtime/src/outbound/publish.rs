//! `[PUBLISH:app_id]` marker processing — cover generation + OA draft creation.

use tracing::{error, info, warn};

use crate::kernel_handle::KernelHandle;

use super::parse::{parse_publish_content, parse_publish_markers};
use super::types::ChannelSendFn;

/// For each marker, spawns the reliable publish handler (cover → draft →
/// publish) in the background; the marker is stripped from the text. Returns
/// the cleaned text with all PUBLISH markers removed.
///
/// `send_fn` + channel routing are only used for the post-publish follow-up
/// notification — the draft itself is created via the kernel/WeChat API, so
/// passing `send_fn = None` still publishes (just without a follow-up message).
///
/// Shared by both the interactive reply path (`send_response`) and the cron
/// delivery path (`cron_deliver_response`), so scheduled publishes create
/// drafts exactly like inline ones (previously cron bypassed this and the
/// marker was shipped as raw text, never publishing).
pub fn process_publish_markers(
    kernel: std::sync::Arc<dyn KernelHandle>,
    send_fn: Option<ChannelSendFn>,
    channel_type: &str,
    bot_id: &str,
    sender_id: &str,
    agent_id: &str,
    response: &str,
) -> String {
    let (publishes, cleaned) = parse_publish_markers(response);
    for (app_id, content) in &publishes {
        // Parse "html_path|title|digest" — title and digest are optional.
        let (html_path, explicit_title, digest) = parse_publish_content(content);
        let digest = digest.filter(|d| !d.is_empty());
        info!(
            %app_id, %html_path, title_provided = explicit_title.is_some(),
            digest_provided = digest.is_some(), %agent_id,
            "PUBLISH marker matched, spawning publish handler"
        );
        let kernel = kernel.clone();
        let send_fn = send_fn.clone();
        let channel_type = channel_type.to_string();
        let bot_id = bot_id.to_string();
        let sender_id = sender_id.to_string();
        let agent_id = agent_id.to_string();
        let app_id = app_id.clone();
        let html_path = html_path.clone();
        tokio::spawn(async move {
            handle_publish_marker(
                kernel,
                send_fn,
                &channel_type,
                &bot_id,
                &sender_id,
                &app_id,
                &html_path,
                explicit_title.as_deref(),
                digest.as_deref(),
                &agent_id,
            )
            .await;
        });
    }
    cleaned
}

/// Read the app_secret for `app_id` from the sender's own profile.json
/// (preferences.wechat_accounts array). Multi-user: each user's OA credentials
/// live in their own directory; find the matching entry by app_id. Returns
/// None if the profile or that account isn't configured.
fn read_wechat_app_secret(
    home: &std::path::Path,
    sender_id: &str,
    agent_id: &str,
    app_id: &str,
) -> Option<String> {
    let profile_path =
        types::config::sender_data_dir(home, sender_id, agent_id, Some(sender_id))
            .join("profile.json");
    let content = std::fs::read_to_string(&profile_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let accounts = v["preferences"]["wechat_accounts"].as_array()?;
    for acct in accounts {
        if acct["app_id"].as_str() == Some(app_id) {
            return acct["app_secret"].as_str().map(|s| s.to_string());
        }
    }
    None
}

/// Recursively search for a file by name under a directory.
/// Returns the most recently modified match as an absolute path string.
fn find_file_recursive(dir: &std::path::Path, filename: &str) -> Option<String> {
    let mut best: Option<(String, std::time::SystemTime)> = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_file_recursive(&path, filename) {
                    // The recursive call already picked the newest in that subtree;
                    // we just need to get its mtime for comparison.
                    if let Ok(meta) = std::fs::metadata(&found) {
                        let mtime = meta.modified().ok()?;
                        if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
                            best = Some((found, mtime));
                        }
                    }
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some(filename) {
                if let Ok(meta) = std::fs::metadata(&path) {
                    if let Ok(mtime) = meta.modified() {
                        if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
                            best = Some((path.to_string_lossy().to_string(), mtime));
                        }
                    }
                }
            }
        }
    }
    best.map(|(p, _)| p)
}

/// Resolve the article title: first non-empty line of the sibling `.md` file
/// (with leading `#` stripped), else the html filename stem.
fn resolve_article_title(html_path: &str) -> String {
    let p = std::path::Path::new(html_path);
    let md = p.with_extension("md");
    if let Ok(content) = std::fs::read_to_string(&md) {
        // Skip metadata-like lines (e.g. "流水线ID: ...") and find the first
        // markdown heading (# title) or the first non-metadata content line.
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Markdown heading — use it as the title
            if trimmed.starts_with('#') {
                let t = trimmed.trim_start_matches('#').trim();
                if !t.is_empty() {
                    return t.to_string();
                }
            }
            // Skip metadata-like lines (key: value patterns, e.g. "流水线ID: ...")
            if trimmed.contains(':')
                && !trimmed.starts_with('-')
                && trimmed
                    .chars()
                    .take(20)
                    .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == ' ' || c >= '\u{4e00}')
            {
                continue;
            }
            // First real content line — use it as title
            return trimmed.to_string();
        }
    }
    // Also try HTML <title> tag
    if let Ok(html) = std::fs::read_to_string(html_path) {
        if let Some(title) = html
            .split("<title>")
            .nth(1)
            .and_then(|s| s.split("</title>").next())
        {
            let t = title.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("未命名文章")
        .to_string()
}

/// Handle a `[PUBLISH:app_id]html_path|digest[/PUBLISH]` marker: generate a
/// cover, create a WeChat OA draft, and publish it — all via in-process API
/// (no MCP, no agent tool-chain; the "AI + API" pattern). The `|digest` part
/// is optional; if omitted, WeChat auto-extracts a digest from the article.
/// Replies to the user with the result once it completes.
#[allow(clippy::too_many_arguments)]
async fn handle_publish_marker(
    kernel: std::sync::Arc<dyn KernelHandle>,
    send_fn: Option<ChannelSendFn>,
    channel_type: &str,
    bot_id: &str,
    sender_id: &str,
    app_id: &str,
    html_path: &str,
    explicit_title: Option<&str>,
    digest: Option<&str>,
    agent_id: &str,
) {
    // Resolve html_path to absolute, mirroring how the agent's file_read
    // resolves relative paths: under the per-sender workspace
    // (workspaces/<agent>/senders/<sender>/), NOT ~/.opencarrier. Absolute
    // paths are used as-is.
    let home = kernel.home_dir().unwrap_or_default();
    let abs_html = if std::path::Path::new(html_path).is_absolute() {
        html_path.to_string()
    } else {
        let base = types::config::sender_data_dir(&home, sender_id, agent_id, Some(sender_id));
        let direct = base.join(html_path);
        if direct.exists() {
            direct.to_string_lossy().to_string()
        } else {
            // Path not found — try resolving just the filename under output/.
            // AI often writes files to output/<pipeline-dir>/filename.html but
            // the PUBLISH marker may reference a different <pipeline-dir>.
            // By searching by filename only, the path mismatch is eliminated.
            let filename = std::path::Path::new(html_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(html_path);
            let output_dir = base.join("output");
            if output_dir.exists() {
                if let Some(found) = find_file_recursive(&output_dir, filename) {
                    info!(original = %html_path, resolved = %found, "PUBLISH: resolved HTML by filename under output/");
                    found
                } else {
                    direct.to_string_lossy().to_string()
                }
            } else {
                direct.to_string_lossy().to_string()
            }
        }
    };

    let title = match explicit_title.filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => resolve_article_title(&abs_html),
    };
    let cover_prompt = format!(
        "WeChat official account article cover image, theme: {title}, flat illustration style, vibrant, clean, no text"
    );

    // Generate cover into the article's directory. On failure, omit cover_path
    // and let the publish tool fall back to the material library.
    let out_dir = std::path::Path::new(&abs_html)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let cover_path = match kernel
        .generate_image_to_file(&cover_prompt, &out_dir.to_string_lossy())
        .await
    {
        Ok(p) => {
            info!(cover = %p, "Cover generated for publish");
            Some(p)
        }
        Err(e) => {
            warn!(error = %e, "Cover generation failed; publish tool will try material-library fallback");
            None
        }
    };

    // Read app_secret from the user's OWN profile (multi-user: each user's OA
    // credentials live in their own directory). Find by app_id in the
    // wechat_accounts array. Empty if not configured — the tool reports it.
    let app_secret = read_wechat_app_secret(&home, sender_id, agent_id, app_id);

    // Drive the publish tool deterministically.
    let ctx = types::plugin::PluginToolContext {
        bot_id: bot_id.to_string(),
        sender_id: sender_id.to_string(),
        agent_id: agent_id.to_string(),
        channel_type: channel_type.to_string(),
    };
    // Draft-only by design: AI-generated content must be human-reviewed before
    // going public, so we never auto-publish (freepublish). The tool creates the
    // draft (cover + content); a human publishes from the OA backend after
    // review. This also avoids the 48001 "api unauthorized" gate that
    // freepublish requires a verified service account for.
    let mut args = serde_json::json!({
        "app_id": app_id,
        "app_secret": app_secret.unwrap_or_default(),
        "html_path": abs_html,
        "title": title,
        "publish": false,
    });
    if let Some(d) = digest {
        if !d.is_empty() {
            args["digest"] = serde_json::Value::String(d.to_string());
        }
    }
    if let Some(cp) = cover_path {
        args["cover_path"] = serde_json::Value::String(cp);
    }

    // The publish tool internally block_on's its own runtime (like the other OA
    // tools), so it MUST run on a spawn_blocking thread — calling it directly on
    // an async runtime worker panics ("cannot start a runtime from within a runtime").
    let tool_result = tokio::task::spawn_blocking(move || {
        kernel.execute_plugin_tool("weixin_oa_publish_article", &args, &ctx)
    })
    .await;

    let result_msg = match tool_result {
        Ok(Some(Ok(body))) => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let media_id = v["media_id"].as_str().unwrap_or("?");
            let cover_src = v["cover_source"].as_str().unwrap_or("?");
            if let Some(pid) = v["publish_id"].as_str() {
                info!(%app_id, %media_id, %pid, cover_source = %cover_src, "Article published via PUBLISH marker");
                format!(
                    "✅ 文章已发布\n《{title}》\n封面来源:{cover_src}\nmedia_id:{media_id}\npublish_id:{pid}"
                )
            } else if let Some(err) = v["publish_error"].as_str() {
                warn!(%app_id, %media_id, error = %err, "Draft created but freepublish failed");
                format!(
                    "⚠️ 草稿已建,但自动发布失败\n《{title}》\n草稿 media_id:{media_id}\n失败原因:{err}\n→ 请到公众号后台草稿箱手动发布(此账号可能无 freepublish 权限,需认证服务号)"
                )
            } else {
                info!(%app_id, %media_id, cover_source = %cover_src, "Draft created (awaiting human review)");
                format!("✅ 草稿已建,待审核\n《{title}》\n封面来源:{cover_src}\n草稿 media_id:{media_id}\n→ 请到公众号后台草稿箱审核后发布")
            }
        }
        Ok(Some(Err(e))) => {
            error!(%app_id, error = %e, "Publish tool failed");
            format!("❌ 发布失败:{e}")
        }
        Ok(None) => {
            error!(%app_id, "weixin_oa_publish_article tool not registered in dispatcher");
            "❌ 发布失败:publish 工具未注册".to_string()
        }
        Err(join_err) => {
            error!(%app_id, error = %join_err, "Publish task panicked");
            "❌ 发布失败:内部任务异常".to_string()
        }
    };

    // Push the result back to the user as a follow-up message.
    if let Some(send_fn) = send_fn {
        let channel_type = channel_type.to_string();
        let bot_id = bot_id.to_string();
        let sender_id = sender_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = send_fn(&channel_type, &bot_id, &sender_id, &result_msg) {
                error!(%channel_type, %sender_id, error = %e, "Publish result reply failed");
            }
        })
        .await;
    }
}
