//! Shared WeChat Official Account API core — the "one core" of the
//! one-core-two-skins layout (2026-08-18).
//!
//! Historically the WeChat client lived inside `channels/weixin-oa` and three
//! parallel token/credential shells grew around it (channel account cache, an
//! api-routes cache, and the publish tool's per-call fetch). This crate is the
//! convergence point: it depends only on `types`, so the channel adapter, the
//! api server, and the kernel daemon can all call it without layering cycles.
//!
//!   - [`api`]: the WeChat HTTP client (single copy in the workspace)
//!   - [`token`]: central stable_token cache with per-app single-flight
//!   - [`session`]: senders/<app_id>/session.json schema + loader
//!   - [`publish_tracker`]: pending freepublish ids polled by the daemon
//!
//! Zero-LLM by construction: every consumer here is a deterministic path
//! (cron arms, channel delivery, HTTP endpoints) — no agent tooling.

pub mod api;
pub mod publish_tracker;
pub mod session;
pub mod token;
