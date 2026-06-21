//! Shared application state for all route handlers.

use kernel::CarrierKernel;
use runtime::channel_manager::ChannelManager;
use std::sync::Arc;
use std::time::Instant;

/// Shared application state.
///
/// The kernel is wrapped in Arc so it can serve as both the main kernel
/// and the KernelHandle for inter-agent tool access.
pub struct AppState {
    pub kernel: Arc<CarrierKernel>,
    pub started_at: Instant,
    /// Notify handle to trigger graceful HTTP server shutdown from the API.
    pub shutdown_notify: Arc<tokio::sync::Notify>,
    /// Channel manager (optional — only if channels are configured).
    #[allow(clippy::type_complexity)]
    pub channel_manager: Option<Arc<tokio::sync::Mutex<ChannelManager>>>,
}
