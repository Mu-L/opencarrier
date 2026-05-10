//! WeChat iLink built-in channel adapter and tools.
//!
//! Provides:
//! - `ILinkChannel` — per-bot long-polling message receiver
//! - `SessionWatcher` — dynamic tenant discovery and polling
//! - `WeixinQrLoginTool` — trigger QR code login
//! - `WeixinSendMessageTool` — send messages to WeChat users
//! - `WeixinStatusTool` — show bot status

pub mod api;
pub mod auth;
pub mod channel;
pub mod crypto;
pub mod token;
pub mod tools;
pub mod types;

pub use channel::{ILinkChannel, SessionWatcher};
pub use tools::{WeixinQrLoginTool, WeixinSendMessageTool, WeixinStatusTool};
