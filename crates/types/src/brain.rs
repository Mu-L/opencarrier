//! Brain configuration types — the carrier's LLM brain.
//!
//! Three-layer architecture:
//! - **Provider**: identity + credentials (name + API key)
//! - **Endpoint**: complete callable unit (provider + model + base_url)
//! - **Modality**: task type → endpoint with fallback chain
//!
//! All LLM traffic goes through aginxbrain (OpenAI-compatible proxy),
//! so format is always "openai" and auth is always Bearer token.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level brain configuration, deserialized from `brain.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainConfig {
    /// Providers: name → credentials.
    pub providers: HashMap<String, ProviderConfig>,
    /// Endpoints: name → complete callable unit.
    pub endpoints: HashMap<String, EndpointConfig>,
    /// Modalities: task type → endpoint routing.
    pub modalities: HashMap<String, ModalityConfig>,
    /// Default modality when agent doesn't specify one.
    #[serde(default = "default_modality")]
    pub default_modality: String,
}

fn default_modality() -> String {
    "chat".to_string()
}

/// Provider = identity + credentials.
///
/// Only knows name and how to authenticate. No URLs, no formats, no models.
/// All providers use simple API key auth (Bearer token) — aginxbrain handles
/// any provider-specific authentication on the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Environment variable name holding the API key.
    /// If empty/missing, this provider doesn't require authentication (e.g., local aginxbrain).
    #[serde(default)]
    pub api_key_env: String,
}

/// Endpoint = base_url + model (complete callable unit).
///
/// Contains everything needed to make an LLM API call:
/// - Which provider to get credentials from
/// - Which model (tag) to request — for aginxbrain, this is a routing tag
/// - Where to send the request (base_url)
///
/// Format is always OpenAI (aginxbrain proxy). Auth is always Bearer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// Provider name — used to look up API key.
    pub provider: String,
    /// Model identifier / routing tag (e.g., "chat", "reasoning", "tts").
    /// For aginxbrain, this is a tag that routes to the appropriate backend.
    pub model: String,
    /// Complete API base URL.
    pub base_url: String,
    /// Legacy fields accepted from old brain.json but no longer used.
    /// All endpoints use OpenAI format through aginxbrain.
    #[serde(default, skip_serializing)]
    pub format: String,
    #[serde(default, skip_serializing)]
    pub auth_header: String,
}

/// Modality = task type → endpoint routing.
///
/// Maps a capability (chat, vision, code, etc.) to a primary endpoint
/// with optional fallback chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModalityConfig {
    /// Primary endpoint name.
    pub primary: String,
    /// Fallback endpoint names, tried in order on failure.
    #[serde(default)]
    pub fallbacks: Vec<String>,
    /// Human-readable description of this modality.
    #[serde(default)]
    pub description: String,
}

// ---------------------------------------------------------------------------
// Brain query types — returned by the Brain trait methods
// ---------------------------------------------------------------------------

/// A resolved endpoint returned by `Brain::endpoints_for()`.
///
/// Contains everything the runtime needs to call this endpoint,
/// without the driver itself (driver is fetched separately via
/// `Brain::driver_for_endpoint()`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedEndpoint {
    /// Endpoint name (the key from brain.json endpoints).
    pub id: String,
    /// Model name to set in CompletionRequest.model.
    pub model: String,
    /// Provider name (for logging / health tracking).
    pub provider: String,
}

/// Information about a modality, returned by `Brain::list_modalities()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModalityInfo {
    /// Modality name (e.g., "chat", "fast", "vision").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Primary endpoint name.
    pub primary_endpoint: String,
    /// Number of fallback endpoints.
    pub fallback_count: usize,
}

/// Feedback from the runtime to Brain after an endpoint call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointReport {
    /// Which endpoint was attempted.
    pub endpoint_id: String,
    /// Whether the call succeeded.
    pub success: bool,
    /// Call latency in milliseconds.
    pub latency_ms: u64,
    /// Error message if the call failed.
    pub error: Option<String>,
}

/// Health status of a single endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointHealth {
    /// Endpoint name.
    pub endpoint: String,
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Whether the driver was successfully created at boot.
    pub driver_ready: bool,
    /// Total successful calls (from report()).
    pub success_count: u64,
    /// Total failed calls (from report()).
    pub failure_count: u64,
    /// Average latency in ms (0 if no data).
    pub avg_latency_ms: u64,
    /// Consecutive failures (reset to 0 on success).
    pub consecutive_failures: u32,
    /// Whether the circuit-breaker has opened (endpoint taken out of rotation).
    pub circuit_open: bool,
}

/// Overall Brain status snapshot, returned by `Brain::status()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainStatus {
    /// All modalities.
    pub modalities: Vec<ModalityInfo>,
    /// Health of all endpoints.
    pub endpoints: Vec<EndpointHealth>,
    /// Number of drivers that initialized successfully.
    pub drivers_ready: usize,
}

/// Resolved credentials for a provider, ready for injection into skill subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredentials {
    /// Provider name.
    pub provider_name: String,
    /// Environment variable name → resolved value pairs.
    /// e.g., {"KLING_ACCESS_KEY": "xxx", "KLING_SECRET_KEY": "yyy"}
    pub env_vars: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_brain_config_parse() {
        let json = r#"{
            "providers": {
                "aginxbrain": { "api_key_env": "AGINXBRAIN_API_KEY" },
                "ollama": {}
            },
            "endpoints": {
                "brain_chat": {
                    "provider": "aginxbrain",
                    "model": "chat",
                    "base_url": "https://brain.aginx.net/v1/chat/completions"
                },
                "brain_reasoning": {
                    "provider": "aginxbrain",
                    "model": "reasoning",
                    "base_url": "https://brain.aginx.net/v1/chat/completions"
                },
                "ollama_local": {
                    "provider": "ollama",
                    "model": "llama3.2:latest",
                    "base_url": "http://localhost:11434/v1"
                }
            },
            "modalities": {
                "chat": {
                    "primary": "brain_chat",
                    "fallbacks": ["brain_reasoning"]
                },
                "fast": {
                    "primary": "ollama_local"
                }
            }
        }"#;

        let config: BrainConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.providers["aginxbrain"].api_key_env, "AGINXBRAIN_API_KEY");
        assert!(config.providers["ollama"].api_key_env.is_empty());

        assert_eq!(config.endpoints.len(), 3);
        assert_eq!(config.endpoints["brain_chat"].model, "chat");

        assert_eq!(config.modalities["chat"].primary, "brain_chat");
        assert_eq!(config.modalities["chat"].fallbacks, vec!["brain_reasoning"]);
        assert!(config.modalities["fast"].fallbacks.is_empty());

        assert_eq!(config.default_modality, "chat"); // default
    }

    #[test]
    fn test_legacy_endpoint_fields_ignored() {
        // Old brain.json with "format": "anthropic" and "auth_header": "api_key" still parses
        let json = r#"{
            "provider": "aginxbrain",
            "model": "chat",
            "base_url": "https://brain.aginx.net/v1/chat/completions",
            "format": "anthropic",
            "auth_header": "api_key"
        }"#;
        let ep: EndpointConfig = serde_json::from_str(json).unwrap();
        assert_eq!(ep.model, "chat");
        // format and auth_header are accepted but skip_serializing
        assert_eq!(ep.format, "anthropic");
        assert_eq!(ep.auth_header, "api_key");
    }

    #[test]
    fn test_legacy_provider_fields_ignored() {
        // Old provider config with auth_type/params still parses
        let json = r#"{
            "api_key_env": "FOO",
            "auth_type": "jwt",
            "params": {"access_key_env": "AK", "secret_key_env": "SK"}
        }"#;
        let pc: ProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(pc.api_key_env, "FOO");
    }
}
