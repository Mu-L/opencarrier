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
    use types::brain::{ApiFormat, AuthHeaderType};

    #[test]
    fn test_anthropic_format_with_key_and_url() {
        let config = DriverConfig {
            provider: "anthropic".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: Some("https://api.anthropic.com/v1/messages".to_string()),
            format: Some(ApiFormat::Anthropic),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "Anthropic format with key + URL should succeed"
        );
    }

    #[test]
    fn test_anthropic_format_no_key_errors() {
        let config = DriverConfig {
            provider: "anthropic".to_string(),
            api_key: None,
            base_url: Some("https://api.anthropic.com/v1/messages".to_string()),
            format: Some(ApiFormat::Anthropic),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let result = create_driver(&config);
        assert!(result.is_err(), "Anthropic format without key should error");
    }

    #[test]
    fn test_anthropic_format_no_url_errors() {
        let config = DriverConfig {
            provider: "anthropic".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: None,
            format: Some(ApiFormat::Anthropic),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let result = create_driver(&config);
        assert!(result.is_err(), "Anthropic format without URL should error");
    }

    #[test]
    fn test_openai_format_with_key_and_url() {
        let config = DriverConfig {
            provider: "groq".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: Some("https://api.groq.com/openai/v1/chat/completions".to_string()),
            format: Some(ApiFormat::OpenAI),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "OpenAI format with key + URL should succeed"
        );
    }

    #[test]
    fn test_openai_format_no_key_succeeds() {
        // OpenAI format does not require API key (e.g. Ollama)
        let config = DriverConfig {
            provider: "ollama".to_string(),
            api_key: None,
            base_url: Some("http://localhost:11434/v1/chat/completions".to_string()),
            format: Some(ApiFormat::OpenAI),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "OpenAI format without key should succeed (local providers)"
        );
    }

    #[test]
    fn test_openai_format_no_url_errors() {
        let config = DriverConfig {
            provider: "openai".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: None,
            format: Some(ApiFormat::OpenAI),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let result = create_driver(&config);
        assert!(result.is_err(), "OpenAI format without URL should error");
    }

    #[test]
    fn test_gemini_format_with_key_and_url() {
        let config = DriverConfig {
            provider: "gemini".to_string(),
            api_key: Some("test-key".to_string()),
            base_url: Some("https://generativelanguage.googleapis.com/v1beta/models".to_string()),
            format: Some(ApiFormat::Gemini),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "Gemini format with key + URL should succeed"
        );
    }

    #[test]
    fn test_gemini_format_no_key_errors() {
        let config = DriverConfig {
            provider: "gemini".to_string(),
            api_key: None,
            base_url: Some("https://generativelanguage.googleapis.com".to_string()),
            format: Some(ApiFormat::Gemini),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let result = create_driver(&config);
        assert!(result.is_err(), "Gemini format without key should error");
    }

    #[test]
    fn test_azure_driver_with_key_and_url() {
        let config = DriverConfig {
            provider: "azure".to_string(),
            api_key: Some("test-azure-key".to_string()),
            base_url: Some("https://myresource.openai.azure.com/openai/deployments".to_string()),
            format: Some(ApiFormat::OpenAI),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(driver.is_ok(), "Azure driver with key + URL should succeed");
    }

    #[test]
    fn test_azure_driver_no_url_errors() {
        let config = DriverConfig {
            provider: "azure".to_string(),
            api_key: Some("test-azure-key".to_string()),
            base_url: None,
            format: Some(ApiFormat::OpenAI),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let result = create_driver(&config);
        assert!(result.is_err(), "Azure driver without URL should error");
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("base_url"),
            "Error should mention base_url: {}",
            err
        );
    }

    #[test]
    fn test_azure_openai_alias_driver_creation() {
        let config = DriverConfig {
            provider: "azure-openai".to_string(),
            api_key: Some("test-azure-key".to_string()),
            base_url: Some("https://myresource.openai.azure.com/openai/deployments".to_string()),
            format: Some(ApiFormat::OpenAI),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "azure-openai alias should create driver successfully"
        );
    }

    #[test]
    fn test_kimi_coding_anthropic_format() {
        // kimi_coding with Anthropic format should use AnthropicDriver
        let config = DriverConfig {
            provider: "kimi".to_string(),
            api_key: Some("test-kimi-key".to_string()),
            base_url: Some("https://api.kimi.com/coding/v1/messages".to_string()),
            format: Some(ApiFormat::Anthropic),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "kimi_coding with Anthropic format should succeed"
        );
    }

    #[test]
    fn test_claude_code_cli_driver() {
        let config = DriverConfig {
            provider: "claude-code".to_string(),
            api_key: None,
            base_url: Some("/usr/local/bin/claude".to_string()),
            format: None,
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(
            driver.is_ok(),
            "claude-code provider should create CLI driver"
        );
    }

    #[test]
    fn test_unknown_provider_openai_format() {
        // Any provider with OpenAI format and base_url should work
        let config = DriverConfig {
            provider: "my-custom-llm".to_string(),
            api_key: Some("test".to_string()),
            base_url: Some("http://localhost:9999/v1/chat/completions".to_string()),
            format: Some(ApiFormat::OpenAI),
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(driver.is_ok());
    }

    #[test]
    fn test_default_format_is_openai() {
        // When format is None, defaults to OpenAI
        let config = DriverConfig {
            provider: "custom".to_string(),
            api_key: Some("test".to_string()),
            base_url: Some("http://localhost:1234/v1/chat/completions".to_string()),
            format: None,
            auth_header: AuthHeaderType::default(),
            skip_permissions: true,
        };
        let driver = create_driver(&config);
        assert!(driver.is_ok(), "Default format (OpenAI) should work");
    }
}
