//! Global Activity Digest tree.
//!
//! One tree per owner, built end-of-day from source tree summaries so a
//! question like "what did I do in the last 7 days?" can be answered with
//! one summary hop.
//!
//! Level conventions (time-axis aligned):
//!   - L0 = one node per **day**
//!   - L1 = one node per **week** (~7 daily leaves)
//!   - L2 = one node per **month** (~4 weekly nodes)
//!   - L3 = one node per **year** (~12 monthly nodes)

pub mod digest;
pub mod hotness;

/// Number of L0 (daily) nodes that seal into one L1 (weekly) node.
pub const WEEKLY_SEAL_THRESHOLD: usize = 7;

/// Number of L1 (weekly) nodes that seal into one L2 (monthly) node.
pub const MONTHLY_SEAL_THRESHOLD: usize = 4;

/// Number of L2 (monthly) nodes that seal into one L3 (yearly) node.
pub const YEARLY_SEAL_THRESHOLD: usize = 12;

/// Literal scope used for the singleton global tree.
pub const GLOBAL_SCOPE: &str = "global";

/// Token budget for global-tree summariser output.
pub const GLOBAL_TOKEN_BUDGET: u32 = 4_000;
