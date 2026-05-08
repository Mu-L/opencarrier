//! Memory substrate for the Carrier Agent Operating System.
//!
//! Provides a unified memory API over three storage backends:
//! - **Structured store** (SQLite): Key-value pairs, sessions, agent state
//! - **Semantic store**: Text-based search (Phase 1: LIKE matching, Phase 2: Qdrant vectors)
//! - **Knowledge graph** (SQLite): Entities and relations
//!
//! Agents interact with a single `Memory` trait that abstracts over all three stores.

pub mod acp_session;
pub mod consolidation;
pub mod invites;
pub mod knowledge;
pub mod migration;
pub mod semantic;
pub mod session;
pub mod structured;
pub mod usage;

mod substrate;
pub use invites::InviteStore;
pub use semantic::SemanticStore;
pub use session::SessionStore;
pub use substrate::MemorySubstrate;
