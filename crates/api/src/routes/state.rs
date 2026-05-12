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
    /// Probe cache for local provider health checks (ollama/vllm/lmstudio).
    /// Avoids blocking the `/api/providers` endpoint on TCP timeouts to
    /// unreachable local services. 60-second TTL.
    pub provider_probe_cache: runtime::provider_health::ProbeCache,
    /// Channel manager (optional — only if channels are configured).
    #[allow(clippy::type_complexity)]
    pub channel_manager: Option<Arc<tokio::sync::Mutex<ChannelManager>>>,
}
