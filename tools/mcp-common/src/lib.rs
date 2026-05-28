//! Shared utilities for OpenCarrier MCP servers.
//!
//! Provides auth macros (cookie-based and app_id/secret-based), JSON helpers,
//! and a generic HTTP API client so individual MCP servers don't duplicate
//! boilerplate.

pub mod api;
pub mod app_auth;
pub mod cookie;
pub mod json;
pub mod ssrf;
