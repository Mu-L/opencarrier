//! senders/&lt;app_id&gt;/session.json — the server-side OA account schema.
//!
//! Moved here from `channels/weixin-oa/src/models.rs` (2026-08-18): both the
//! channel adapter and the api routes / kernel daemon need to read it, and the
//! channel crate re-exports the struct so every existing import keeps
//! compiling. All new fields are `#[serde(default)]` so existing session files
//! and the agent_channels.rs writer round-trip unchanged.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinOaSessionFile {
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default = "default_sender_key")]
    pub sender_key: String,
    #[serde(default)]
    pub name: String,
    pub app_id: String,
    pub app_secret: String,
    /// WeChat OA Token — shared secret used for checkSign signature verification.
    /// Configured alongside the server URL in the 公众号后台.
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub wechat_id: String,
    #[serde(default)]
    pub bind_agent: Option<String>,
    /// Optional 86bus `bind-openid` endpoint. When set, the weixin-oa webhook
    /// POSTs `{ "openid_sa": <from_user> }` on each inbound message so the 86bus
    /// backend can associate the service-account openid with a business identity.
    /// The returned `matched` role is cached and surfaced to the agent.
    #[serde(default)]
    pub bind_openid_url: Option<String>,
    /// Template-message fallback when a customer-service reply hits the 48h
    /// window (WeChat errcode 45015). When both this and
    /// [`Self::fallback_template_field`] are set, `deliver_oa` retries the text
    /// as a template message: `data = {field: {value: <text>}}`. Template
    /// messages have no 48h-window limit (the 86bus account is a verified
    /// service account with 26 templates in library).
    #[serde(default)]
    pub fallback_template_id: Option<String>,
    /// Which template data key carries the fallback text (e.g. `"thing1"` /
    /// `"character_string1"` — depends on the chosen template's field layout).
    #[serde(default)]
    pub fallback_template_field: Option<String>,
}

fn default_channel() -> String {
    "weixin-oa".to_string()
}

fn default_sender_key() -> String {
    "app_id".to_string()
}

/// Load the server-bound OA account session for `app_id` from
/// `<home>/senders/<app_id>/session.json`.
///
/// Generalizes the api-routes `load_session` (webhook callback + bind path)
/// so the daemon (FollowerReport / PublishPoll) and the new admin endpoints
/// resolve credentials through the same single reader. Returns `None` when the
/// file is missing, unparsable, or belongs to another channel/app.
pub fn load_account(home: &std::path::Path, app_id: &str) -> Option<WeixinOaSessionFile> {
    let path = home.join("senders").join(app_id).join("session.json");
    let data = std::fs::read_to_string(path).ok()?;
    let session: WeixinOaSessionFile = serde_json::from_str(&data).ok()?;
    if session.channel == "weixin-oa" && session.app_id == app_id {
        Some(session)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("oc-wechat-oa-session-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// Old-format session files (written before the fallback_template fields
    /// existed) must keep parsing — they are live on the server today.
    #[test]
    fn old_session_file_without_new_fields_parses() {
        let old = r#"{"channel":"weixin-oa","app_id":"wxAAA","app_secret":"s","token":"t"}"#;
        let sf: WeixinOaSessionFile = serde_json::from_str(old).unwrap();
        assert_eq!(sf.app_id, "wxAAA");
        assert_eq!(sf.fallback_template_id, None);
        assert_eq!(sf.fallback_template_field, None);
    }

    #[test]
    fn new_fields_round_trip() {
        let sf = WeixinOaSessionFile {
            fallback_template_id: Some("tpl-1".into()),
            fallback_template_field: Some("thing1".into()),
            ..serde_json::from_str(r#"{"app_id":"wxAAA","app_secret":"s"}"#).unwrap()
        };
        let json = serde_json::to_string(&sf).unwrap();
        let back: WeixinOaSessionFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.fallback_template_id.as_deref(), Some("tpl-1"));
        assert_eq!(back.fallback_template_field.as_deref(), Some("thing1"));
    }

    #[test]
    fn load_account_filters_wrong_channel_and_app() {
        let home = tmp_home("load");
        let dir = home.join("senders/wxAAA");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session.json"),
            r#"{"channel":"weixin-oa","app_id":"wxAAA","app_secret":"s"}"#,
        )
        .unwrap();
        let dir2 = home.join("senders/wxBBB");
        std::fs::create_dir_all(&dir2).unwrap();
        // Right path, wrong embedded channel — must be rejected.
        std::fs::write(
            dir2.join("session.json"),
            r#"{"channel":"wecom","app_id":"wxBBB","app_secret":"s"}"#,
        )
        .unwrap();

        assert!(load_account(&home, "wxAAA").is_some());
        assert!(load_account(&home, "wxBBB").is_none(), "wrong channel rejected");
        assert!(load_account(&home, "wxNONE").is_none(), "missing file");
        let _ = std::fs::remove_dir_all(&home);
    }
}
