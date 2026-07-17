//! Content delivery descriptors - channel-agnostic rich content with multiple
//! representations.
//!
//! A single piece of content (e.g. "月票") carries several optional
//! representations (`text`, `link`, `image`, `video`, `file`, `miniprogram`).
//! Each channel's [`Channel::deliver`](crate::channel::Channel::deliver) picks
//! the highest-fidelity form it supports; everything degrades to `text`.
//!
//! This lets one content key be delivered as a miniprogram card on a 服务号,
//! a file/video on wecom kf, and a plain link on iLink - same content, best
//! form per channel.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A media reference, resolved from any of: a public `url`, a local `file_path`,
/// or a pre-uploaded platform `media_id`. Channels use whichever they can -
/// `media_id` is only valid on the platform that issued it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaRef {
    /// Public URL the channel can fetch, or pass directly for URL-based media.
    pub url: Option<String>,
    /// Local file path (absolute, or relative to `~/.opencarrier`).
    pub file_path: Option<String>,
    /// Pre-uploaded platform media_id (only valid where it was issued).
    pub media_id: Option<String>,
}

impl MediaRef {
    pub fn is_empty(&self) -> bool {
        self.url.is_none() && self.file_path.is_none() && self.media_id.is_none()
    }
}

/// A link card: title + description + click URL + optional cover image URL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkContent {
    pub title: String,
    pub desc: String,
    pub url: String,
    /// Cover image URL (public; no upload needed).
    pub pic_url: Option<String>,
}

/// A miniprogram card.
///
/// Thumb handling differs per channel: weixin-oa accepts an OA permanent
/// `thumb_media_id` directly; wecom kf uses a *separate* media library and
/// must re-upload from `thumb_url`/`thumb_file` (an OA media_id is invalid
/// there). Descriptors therefore carry both; each channel picks what it can use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MiniprogramContent {
    pub appid: String,
    pub pagepath: String,
    pub title: String,
    /// OA permanent thumb_media_id - only usable on weixin-oa.
    pub thumb_media_id: Option<String>,
    /// Public cover URL - downloaded + re-uploaded per channel (wecom kf).
    pub thumb_url: Option<String>,
    /// Local cover file - uploaded per channel.
    pub thumb_file: Option<String>,
}

/// Channel-agnostic rich content. Each representation is optional; a channel's
/// `deliver()` picks the best one it supports and degrades to `text` otherwise.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentDescriptor {
    /// Plain text - the universal lowest-fidelity fallback.
    pub text: Option<String>,
    pub link: Option<LinkContent>,
    pub image: Option<MediaRef>,
    pub video: Option<MediaRef>,
    pub file: Option<MediaRef>,
    pub voice: Option<MediaRef>,
    pub miniprogram: Option<MiniprogramContent>,
}

impl ContentDescriptor {
    /// The best plain-text rendering (text itself, or a formatted link), for
    /// channels that can only send text (the default `deliver` fallback).
    pub fn as_text(&self) -> Option<String> {
        if let Some(t) = &self.text {
            return Some(t.clone());
        }
        if let Some(l) = &self.link {
            if l.title.is_empty() {
                return Some(l.url.clone());
            }
            return Some(format!("{}\n{}", l.title, l.url));
        }
        None
    }
}

/// One entry in a per-agent content registry: a `key` (what the agent writes in
/// a `[DELIVER:key]` marker) plus the content's representations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeliverableEntry {
    /// Content key referenced by `[DELIVER:<key>]` markers.
    pub key: String,
    #[serde(flatten)]
    pub content: ContentDescriptor,
}

/// Per-agent content registry, loaded from
/// `~/.opencarrier/workspaces/{agent}/content.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentConfig {
    pub deliverables: Vec<DeliverableEntry>,
}

impl ContentConfig {
    /// Look up a content descriptor by key.
    pub fn get(&self, key: &str) -> Option<&ContentDescriptor> {
        self.deliverables
            .iter()
            .find(|d| d.key == key)
            .map(|d| &d.content)
    }

    /// Build a keyed map (for caches that prefer HashMap lookup).
    pub fn into_map(self) -> HashMap<String, ContentDescriptor> {
        self.deliverables
            .into_iter()
            .map(|d| (d.key, d.content))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_toml() {
        let toml = r#"
[[deliverables]]
key = "月票"
text = "这是咱们的月票"
[deliverables.link]
title = "86优团-月票"
url = "https://bus.86xq.com/x"
[deliverables.miniprogram]
appid = "wxabc"
pagepath = "pages/index/type/type?id=883"
title = "86优团-月票"
thumb_media_id = "OA_PERM"
thumb_url = "https://mmecoa.qpic.cn/cover.png"
"#;
        let cfg: ContentConfig = toml::from_str(toml).unwrap();
        let d = cfg.get("月票").expect("月票 entry");
        assert_eq!(d.text.as_deref(), Some("这是咱们的月票"));
        assert_eq!(d.link.as_ref().unwrap().title, "86优团-月票");
        let mp = d.miniprogram.as_ref().unwrap();
        assert_eq!(mp.appid, "wxabc");
        assert_eq!(mp.thumb_media_id.as_deref(), Some("OA_PERM"));
        assert_eq!(mp.thumb_url.as_deref(), Some("https://mmecoa.qpic.cn/cover.png"));
        assert!(cfg.get("不存在").is_none());
    }

    #[test]
    fn as_text_falls_back_to_link() {
        let d = ContentDescriptor {
            link: Some(LinkContent {
                title: "t".into(),
                url: "https://x".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(d.as_text().unwrap(), "t\nhttps://x");
        let d2 = ContentDescriptor {
            text: Some("plain".into()),
            link: Some(LinkContent {
                title: "t".into(),
                url: "https://x".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(d2.as_text().unwrap(), "plain");
    }
}
