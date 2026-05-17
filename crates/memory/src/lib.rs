//! Memory substrate for the Carrier Agent Operating System.
//!
//! Provides tree-based hierarchical memory with Obsidian-compatible content storage,
//! plus system infrastructure (agent registry, sessions, invites, cron delivery).

pub mod acp_session;
pub mod cron_delivery;
pub mod invites;
pub mod migration;
pub mod session;
pub mod system_kv;
pub mod tree;
pub mod usage;

mod substrate;
pub use cron_delivery::CronDeliveryStore;
pub use invites::InviteStore;
pub use session::SessionStore;
pub use substrate::MemorySubstrate;
