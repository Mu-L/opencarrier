//! HTTP/WebSocket API server for the Carrier Agent OS daemon.
//!
//! Exposes agent management, status, and chat via JSON REST endpoints.
//! The kernel runs in-process; the CLI connects over HTTP.

pub mod middleware;
pub mod migration;
pub mod pages;
pub mod rate_limiter;
pub mod routes;
pub mod server;
pub mod session_auth;
pub mod types;
pub mod webchat;
pub mod ws;
