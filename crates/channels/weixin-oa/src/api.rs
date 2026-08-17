//! Re-export shim: the WeChat OA API client moved to the shared `wechat-oa`
//! core crate (2026-08-18 three-shell convergence — channel adapter, api
//! routes, and kernel daemon all call the one copy). Kept as a module so
//! existing `crate::api::…` / `channel_weixin_oa::api::…` imports compile
//! unchanged.

pub use wechat_oa::api::*;
