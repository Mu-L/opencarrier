//! LLM driver implementations.
//!
//! CLI subprocess drivers (claude-code, qwen-code) and fallback chain driver.
//! All HTTP API drivers are handled by `UnifiedHttpDriver` in `llm_driver_impl.rs`.

pub mod claude_code;
pub mod fallback;
pub mod qwen_code;

use crate::llm_driver::{DriverConfig, LlmDriver, LlmError};
use std::sync::Arc;

/// Create an LLM driver based on the format field in configuration.
///
/// Delegates to `llm_driver::create_driver()` which handles both CLI
/// subprocess drivers and HTTP API drivers (via `UnifiedHttpDriver`).
pub fn create_driver(config: &DriverConfig) -> Result<Arc<dyn LlmDriver>, LlmError> {
    crate::llm_driver::create_driver(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_driver_with_key_and_url() {
        let config = DriverConfig {
            provider: "aginxbrain".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: Some("https://brain.aginx.net/v1/chat/completions".to_string()),
            format: None,
            auth_header: types::brain::AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "HTTP driver with key + URL should succeed"
        );
    }

    #[test]
    fn test_http_driver_no_key_succeeds() {
        // HTTP driver does not require API key (e.g. local aginxbrain)
        let config = DriverConfig {
            provider: "local".to_string(),
            api_key: None,
            base_url: Some("http://localhost:8080/v1/chat/completions".to_string()),
            format: None,
            auth_header: types::brain::AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "HTTP driver without key should succeed (local providers)"
        );
    }

    #[test]
    fn test_http_driver_no_url_errors() {
        let config = DriverConfig {
            provider: "aginxbrain".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: None,
            format: None,
            auth_header: types::brain::AuthHeaderType::default(),
            skip_permissions: true,
        };
        let result = create_driver(&config);
        assert!(result.is_err(), "HTTP driver without URL should error");
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("base_url"),
            "Error should mention base_url: {}",
            err
        );
    }

    #[test]
    fn test_claude_code_cli_driver() {
        let config = DriverConfig {
            provider: "claude-code".to_string(),
            api_key: None,
            base_url: Some("/usr/local/bin/claude".to_string()),
            format: None,
            auth_header: types::brain::AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "claude-code provider should create CLI driver"
        );
    }

    #[test]
    fn test_custom_provider() {
        let config = DriverConfig {
            provider: "my-custom-llm".to_string(),
            api_key: Some("test".to_string()),
            base_url: Some("http://localhost:9999/v1/chat/completions".to_string()),
            format: None,
            auth_header: types::brain::AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(driver.is_ok());
    }
}
