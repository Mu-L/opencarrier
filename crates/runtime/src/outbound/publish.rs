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

/// Extract a field from a leading `<!-- ... -->` META header block written by
/// article-writer etc. `key` is the field name without the `META_` prefix
/// (e.g. "TITLE", "AUTHOR", "DIGEST"). Recognizes both `META_<KEY>:` (the
/// writer's format) and `<key>:` (lowercase fallback). Returns None if there is
/// no comment block or it has no such field.
fn extract_meta_field(content: &str, key: &str) -> Option<String> {
    let start = content.find("<!--")?;
    let rest = &content[start..];
    let end = rest.find("-->")?;
    let block = &rest[4..end];
    let meta_prefix = format!("META_{key}:");
    let plain_prefix = format!("{}:", key.to_lowercase());
    for line in block.lines() {
        let trimmed = line.trim();
        let val = trimmed
            .strip_prefix(&meta_prefix)
            .or_else(|| trimmed.strip_prefix(&plain_prefix))
            .map(|s| s.trim());
        if let Some(v) = val {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Extract the title from a leading META header block. See `extract_meta_field`.
fn extract_meta_title(content: &str) -> Option<String> {
    extract_meta_field(content, "TITLE")
}

/// Resolve the article title. Prefer a `META_TITLE:` field in a leading
/// `<!-- ... -->` META header block; fall back to the first markdown heading or
/// content line of the sibling `.md` (skipping the META comment block and
/// `key: value` metadata lines); finally the HTML `<title>` tag or filename stem.
fn resolve_article_title(html_path: &str) -> String {
    let p = std::path::Path::new(html_path);
    let md = p.with_extension("md");
    if let Ok(content) = std::fs::read_to_string(&md) {
        // 1. Leading META header block: `<!--\nMETA_TITLE: ...\n-->`.
        if let Some(title) = extract_meta_title(&content) {
            return title;
        }
        // 2. First markdown heading or real content line, skipping HTML comment
        //    blocks (which may span multiple lines) and `key: value` metadata.
        let mut in_comment = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if in_comment {
                if trimmed.contains("-->") {
                    in_comment = false;
                }
                continue;
            }
            if trimmed.starts_with("<!--") {
                if !trimmed.contains("-->") {
                    in_comment = true;
                }
                continue;
            }
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
    // OA draft author + digest. article-writer writes META_AUTHOR/META_DIGEST
    // precisely for these fields. An explicit digest in the PUBLISH marker
    // wins; otherwise fall back to META_DIGEST. Author has no marker source —
    // only META_AUTHOR. Read the sibling .md once and share across both
    // META extractions (resolve_article_title already reads it for META_TITLE;
    // this avoids a second read for author/digest).
    let md_content =
        std::fs::read_to_string(std::path::Path::new(&abs_html).with_extension("md")).ok();
    let meta_author = md_content.as_deref().and_then(|c| extract_meta_field(c, "AUTHOR"));
    let meta_digest = md_content.as_deref().and_then(|c| extract_meta_field(c, "DIGEST"));
    let author = meta_author;
    let digest = digest
        .filter(|d| !d.is_empty())
        .map(|d| d.to_string())
        .or(meta_digest);
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
    if let Some(a) = author {
        args["author"] = serde_json::Value::String(a);
    }
    if let Some(d) = digest {
        args["digest"] = serde_json::Value::String(d);
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
        Ok(Ok(Some(body))) => {
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
        Ok(Err(e)) => {
            error!(%app_id, error = %e, "Publish tool failed");
            let reason = match e {
                types::error::CarrierError::ToolExecution { reason, .. } => reason,
                other => other.to_string(),
            };
            format!("❌ 发布失败:{reason}")
        }
        Ok(Ok(None)) => {
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

#[cfg(test)]
mod tests {
    use super::{extract_meta_field, extract_meta_title, resolve_article_title};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Write `content` to a unique temp `article.md` and return its path. The
    /// `.html` sibling (which `resolve_article_title` is given) need not exist:
    /// the resolver reads the `.md` first.
    fn tmp_md(content: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("oc_publish_test_{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join("article.md");
        std::fs::write(&md, content).unwrap();
        md
    }

    #[test]
    fn extract_meta_title_prefers_meta_title_field() {
        let content = "<!--\nMETA_TITLE: 我的标题\nMETA_AUTHOR: 小载\n-->\n\n# 正文";
        assert_eq!(extract_meta_title(content).unwrap(), "我的标题");
    }

    #[test]
    fn extract_meta_title_accepts_plain_title_field() {
        let content = "<!-- META\ntitle: 备选格式\n-->";
        assert_eq!(extract_meta_title(content).unwrap(), "备选格式");
    }

    #[test]
    fn extract_meta_title_none_without_title_field() {
        assert!(extract_meta_title("no comment here").is_none());
        // Block present but no title field.
        assert!(extract_meta_title("<!--\nMETA_AUTHOR: x\n-->").is_none());
    }

    #[test]
    fn extract_meta_field_reads_author_and_digest() {
        let content = "<!--\nMETA_TITLE: 我的标题\nMETA_AUTHOR: 张三\nMETA_DIGEST: 这是一段摘要\n-->\n\n# 正文";
        assert_eq!(extract_meta_field(content, "TITLE").unwrap(), "我的标题");
        assert_eq!(extract_meta_field(content, "AUTHOR").unwrap(), "张三");
        assert_eq!(extract_meta_field(content, "DIGEST").unwrap(), "这是一段摘要");
        assert!(extract_meta_field(content, "NOPE").is_none());
    }

    #[test]
    fn resolve_title_from_meta_block() {
        let md = tmp_md("<!--\nMETA_TITLE: 以前怕你不卖芯片\nMETA_AUTHOR: 小载\n-->\n\n# 别的标题\n正文");
        let html = md.with_extension("html");
        assert_eq!(resolve_article_title(html.to_str().unwrap()), "以前怕你不卖芯片");
    }

    #[test]
    fn resolve_title_skips_comment_block_to_heading() {
        // Regression: a leading `<!--` (no META_TITLE) must NOT become the
        // title; the comment block is skipped and the `#` heading is used.
        // Previously this returned `<!--`, producing draft titles like `《<!--》`.
        let md = tmp_md("<!--\nMETA_AUTHOR: 小载\n-->\n\n# 真正的标题\n正文");
        let html = md.with_extension("html");
        assert_eq!(resolve_article_title(html.to_str().unwrap()), "真正的标题");
    }

    #[test]
    fn resolve_title_from_heading_without_meta() {
        let md = tmp_md("# 标题直接开头\n\n正文");
        let html = md.with_extension("html");
        assert_eq!(resolve_article_title(html.to_str().unwrap()), "标题直接开头");
    }
}
