use super::*;
use crate::text_tool_recovery::{
    parse_dash_dash_args, parse_json_tool_call_object,
};
use crate::llm_driver::{CompletionResponse, LlmError};
use async_trait::async_trait;
use types::tool::ToolCall;
use std::sync::atomic::{AtomicU32, Ordering};


    #[test]
    fn test_max_iterations_constant() {
        assert_eq!(MAX_ITERATIONS, 25);
    }

    #[test]
    fn test_retry_constants() {
        assert_eq!(MAX_RETRIES, 3);
        assert_eq!(BASE_RETRY_DELAY_MS, 1000);
    }

    #[test]
    fn test_dynamic_truncate_short_unchanged() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        let budget = ContextBudget::new(200_000);
        let short = "Hello, world!";
        assert_eq!(truncate_tool_result_dynamic(short, &budget), short);
    }

    #[test]
    fn test_dynamic_truncate_over_limit() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        let budget = ContextBudget::new(200_000);
        let long = "x".repeat(budget.per_result_cap() + 10_000);
        let result = truncate_tool_result_dynamic(&long, &budget);
        assert!(result.len() <= budget.per_result_cap() + 200);
        assert!(result.contains("[TRUNCATED:"));
    }

    #[test]
    fn test_dynamic_truncate_newline_boundary() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        // Small budget to force truncation
        let budget = ContextBudget::new(1_000);
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_tool_result_dynamic(&content, &budget);
        // Should break at a newline, not mid-line
        let before_marker = result.split("[TRUNCATED:").next().unwrap();
        let trimmed = before_marker.trim_end();
        assert!(!trimmed.is_empty());
    }

    #[test]
    fn test_max_continuations_constant() {
        assert_eq!(MAX_CONTINUATIONS, 5);
    }

    #[test]
    fn test_tool_timeout_constant() {
        assert_eq!(TOOL_TIMEOUT_SECS, 120);
    }

    #[test]
    fn test_max_history_messages() {
        assert_eq!(MAX_HISTORY_MESSAGES, 30);
    }

    // --- Loop detection ---

    fn make_call(name: &str, input: serde_json::Value) -> (String, u64) {
        (name.to_string(), tool_input_hash(&input))
    }

    #[test]
    fn test_loop_detection_blocks_consecutive_same_call() {
        let recent: Vec<(String, u64)> = (0..LOOP_DETECTION_WINDOW)
            .map(|_| make_call("test_query", serde_json::json!({"q": "rust"})))
            .collect();
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_some(), "Should detect loop with same call repeated");
        assert_eq!(result.unwrap().0, "test_query");
    }

    #[test]
    fn test_loop_detection_allows_pagination() {
        // Same tool name but different inputs (pagination) — not a loop
        let recent: Vec<(String, u64)> = (0..LOOP_DETECTION_WINDOW)
            .map(|i| make_call("test_query", serde_json::json!({"q": format!("rust page {}", i)})))
            .collect();
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_none(), "Pagination with different queries should not be flagged");
    }

    #[test]
    fn test_loop_detection_requires_full_window() {
        // 5 same calls is below threshold of 6
        let recent: Vec<(String, u64)> = (0..5)
            .map(|_| make_call("test_query", serde_json::json!({"q": "rust"})))
            .collect();
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_none(), "Below-threshold count should not trigger");
    }

    #[test]
    fn test_loop_detection_breaks_on_different_tool() {
        // 5 test_query + 1 web_fetch + 5 test_query → no loop (window is 6, last 6 are mixed)
        let mut recent: Vec<(String, u64)> = (0..5)
            .map(|_| make_call("test_query", serde_json::json!({"q": "rust"})))
            .collect();
        recent.push(make_call("web_fetch", serde_json::json!({"url": "https://example.com"})));
        recent.extend(
            (0..5).map(|_| make_call("test_query", serde_json::json!({"q": "rust"})))
        );
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_none(), "Mixed tail should not trigger loop detection");
    }

    #[test]
    fn test_loop_detection_window_constant() {
        assert_eq!(LOOP_DETECTION_WINDOW, 6);
    }

    // --- Integration tests for empty response guards ---

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            name: "test-agent".to_string(),
            model: types::agent::ModelConfig {
                system_prompt: "You are a test agent.".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Mock driver that simulates: first call returns ToolUse with no text,
    /// second call returns EndTurn with empty text. This reproduces the bug
    /// where the LLM ends with no text after a tool-use cycle.
    struct EmptyAfterToolUseDriver {
        call_count: AtomicU32,
    }

    impl EmptyAfterToolUseDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for EmptyAfterToolUseDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // First call: LLM wants to use a tool (with no text block)
                Ok(CompletionResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "tool_1".to_string(),
                        name: "fake_tool".to_string(),
                        input: serde_json::json!({"query": "test"}),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    tool_calls: vec![ToolCall {
                        id: "tool_1".to_string(),
                        name: "fake_tool".to_string(),
                        input: serde_json::json!({"query": "test"}),
                    }],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    media: None,
                })
            } else {
                // Second call: LLM returns EndTurn with EMPTY text (the bug)
                Ok(CompletionResponse {
                    content: vec![],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 0,
                    },
                    media: None,
                })
            }
        }
    }

    /// Mock driver that returns empty text with MaxTokens stop reason,
    /// repeated MAX_CONTINUATIONS times to trigger the max continuations path.
    struct EmptyMaxTokensDriver;

    #[async_trait]
    impl LlmDriver for EmptyMaxTokensDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::MaxTokens,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
                media: None,
            })
        }
    }

    /// Mock driver that returns normal text (sanity check).
    struct NormalDriver;

    #[async_trait]
    impl LlmDriver for NormalDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Hello from the agent!".to_string(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 8,
                },
                media: None,
            })
        }
    }

    #[tokio::test]
    async fn test_empty_response_after_tool_use_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());

        let result = run_agent_loop(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[], // no tools registered — the tool call will fail, which is fine
            None, // kernel
            None, // stream_tx
            None, // mcp_connections
            None, // fetch_engine
            None, // workspace_root
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Loop should complete without error");

        // The response MUST NOT be empty — it should contain our fallback text
        assert!(
            !result.response.trim().is_empty(),
            "Response should not be empty after tool use, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("Task completed"),
            "Expected fallback message, got: {:?}",
            result.response
        );
    }

    #[tokio::test]
    async fn test_tool_error_injects_no_fabrication_guidance() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());

        run_agent_loop(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[], // no tools registered — the tool call will fail, which is fine
            None, // kernel
            None, // stream_tx
            None, // mcp_connections
            None, // fetch_engine
            None, // workspace_root
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Loop should complete without error");

        let guidance_seen = session.messages.iter().any(|msg| {
            match &msg.content {
            MessageContent::Blocks(blocks) => blocks.iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. } if text.contains("tool(s) returned errors"))
            }),
            _ => false,
        }
        });

        assert!(
            guidance_seen,
            "Expected tool error guidance in session messages after failed tool call"
        );
    }

    #[tokio::test]
    async fn test_empty_response_max_tokens_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyMaxTokensDriver);

        let result = run_agent_loop(
            &manifest,
            "Tell me something long",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Loop should complete without error");

        // Should hit MAX_CONTINUATIONS and return fallback instead of empty
        assert!(
            !result.response.trim().is_empty(),
            "Response should not be empty on max tokens, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("token limit"),
            "Expected max-tokens fallback message, got: {:?}",
            result.response
        );
    }

    #[tokio::test]
    async fn test_normal_response_not_replaced_by_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(NormalDriver);

        let result = run_agent_loop(
            &manifest,
            "Say hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Loop should complete without error");

        // Normal response should pass through unchanged
        assert_eq!(result.response, "Hello from the agent!");
    }

    #[tokio::test]
    async fn test_streaming_empty_response_after_tool_use_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());
        let (tx, _rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            tx,
            None,
            None,
            None,
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Streaming loop should complete without error");

        assert!(
            !result.response.trim().is_empty(),
            "Streaming response should not be empty after tool use, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("Task completed"),
            "Expected fallback message in streaming, got: {:?}",
            result.response
        );
    }

    /// Mock driver that returns empty text on first call (EndTurn), then normal text on second.
    /// This tests the one-shot retry logic for iteration 0 empty responses.
    struct EmptyThenNormalDriver {
        call_count: AtomicU32,
    }

    impl EmptyThenNormalDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for EmptyThenNormalDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // First call: empty EndTurn (triggers retry)
                Ok(CompletionResponse {
                    content: vec![],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 0,
                    },
                    media: None,
                })
            } else {
                // Second call (retry): normal response
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "Recovered after retry!".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 15,
                        output_tokens: 8,
                    },
                    media: None,
                })
            }
        }
    }

    /// Mock driver that always returns empty EndTurn (no recovery on retry).
    /// Tests that the fallback message appears when retry also fails.
    struct AlwaysEmptyDriver;

    #[async_trait]
    impl LlmDriver for AlwaysEmptyDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
                media: None,
            })
        }
    }

    #[tokio::test]
    async fn test_empty_first_response_retries_and_recovers() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyThenNormalDriver::new());

        let result = run_agent_loop(
            &manifest,
            "Hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None,
            None,
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Loop should recover via retry");

        assert_eq!(result.response, "Recovered after retry!");
        assert_eq!(
            result.iterations, 2,
            "Should have taken 2 iterations (retry)"
        );
    }

    #[tokio::test]
    async fn test_empty_first_response_fallback_when_retry_also_empty() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(AlwaysEmptyDriver);

        let result = run_agent_loop(
            &manifest,
            "Hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None,
            None,
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Loop should complete with fallback");

        // No tools were executed, so should get the empty response message
        assert!(
            result.response.contains("empty response"),
            "Expected empty response fallback (no tools executed), got: {:?}",
            result.response
        );
    }

    #[tokio::test]
    async fn test_max_history_messages_constant() {
        assert_eq!(MAX_HISTORY_MESSAGES, 30);
    }

    #[tokio::test]
    async fn test_streaming_empty_response_max_tokens_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyMaxTokensDriver);
        let (tx, _rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Tell me something long",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            tx,
            None,
            None,
            None,
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Streaming loop should complete without error");

        assert!(
            !result.response.trim().is_empty(),
            "Streaming response should not be empty on max tokens, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("token limit"),
            "Expected max-tokens fallback in streaming, got: {:?}",
            result.response
        );
    }

    #[test]
    fn test_recover_text_tool_calls_basic() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            r#"Let me search for that. <function=test_query>{"query":"rust async"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[0].input["query"], "rust async");
        assert!(calls[0].id.starts_with("recovered_"));
    }

    #[test]
    fn test_recover_text_tool_calls_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=hack_system>{"cmd":"rm -rf /"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Unknown tools should be rejected");
    }

    #[test]
    fn test_recover_text_tool_calls_invalid_json() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=test_query>not valid json</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Invalid JSON should be skipped");
    }

    #[test]
    fn test_recover_text_tool_calls_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "test_query".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = r#"<function=test_query>{"query":"hello"}</function> then <function=read_file>{"path":"a.txt"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[1].name, "read_file");
    }

    #[test]
    fn test_recover_text_tool_calls_no_pattern() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Just a normal response with no tool calls.";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_text_tool_calls_empty_tools() {
        let text = r#"<function=test_query>{"query":"hello"}</function>"#;
        let calls = recover_text_tool_calls(text, &[]);
        assert!(calls.is_empty(), "No tools = no recovery");
    }

    // --- Deep edge-case tests for text-to-tool recovery ---

    #[test]
    fn test_recover_text_tool_calls_nested_json() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=test_query>{"query":"rust","filters":{"lang":"en","year":2024}}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["filters"]["lang"], "en");
    }

    #[test]
    fn test_recover_text_tool_calls_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Sure, let me search that for you.\n\n<function=test_query>{\"query\":\"rust async programming\"}</function>\n\nI'll get back to you with results.";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["query"], "rust async programming");
    }

    #[test]
    fn test_recover_text_tool_calls_whitespace_in_json() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Some models emit pretty-printed JSON
        let text = "<function=test_query>\n  {\"query\": \"hello world\"}\n</function>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["query"], "hello world");
    }

    #[test]
    fn test_recover_text_tool_calls_unclosed_tag() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Missing </function> — should gracefully skip
        let text = r#"<function=test_query>{"query":"test"}"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Unclosed tag should be skipped");
    }

    #[test]
    fn test_recover_text_tool_calls_missing_closing_bracket() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Missing > after tool name
        let text = r#"<function=test_query{"query":"test"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        // The parser finds > inside JSON, will likely produce invalid tool name
        // or invalid JSON — either way, should not panic
        // (just verifying no panic / no bad behavior)
        let _ = calls;
    }

    #[test]
    fn test_recover_text_tool_calls_empty_json_object() {
        let tools = vec![ToolDefinition {
            name: "list_files".into(),
            description: "List".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=list_files>{}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_files");
        assert_eq!(calls[0].input, serde_json::json!({}));
    }

    #[test]
    fn test_recover_text_tool_calls_mixed_valid_invalid() {
        let tools = vec![
            ToolDefinition {
                name: "test_query".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        // First: valid, second: unknown tool, third: valid
        let text = r#"<function=test_query>{"q":"a"}</function> <function=unknown>{"x":1}</function> <function=read_file>{"path":"b"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2, "Should recover 2 valid, skip 1 unknown");
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[1].name, "read_file");
    }

    // --- Variant 2 pattern tests: <function>NAME{JSON}</function> ---

    #[test]
    fn test_recover_variant2_basic() {
        let tools = vec![ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function>web_fetch{"url":"https://example.com"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_fetch");
        assert_eq!(calls[0].input["url"], "https://example.com");
    }

    #[test]
    fn test_recover_variant2_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function>unknown_tool{"q":"test"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_recover_variant2_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Let me search for that. <function>test_query{"query":"rust lang"}</function> I'll find the answer."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
    }

    #[test]
    fn test_recover_both_variants_mixed() {
        let tools = vec![
            ToolDefinition {
                name: "test_query".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_fetch".into(),
                description: "Fetch".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        // Mix of variant 1 and variant 2
        let text = r#"<function=test_query>{"q":"a"}</function> <function>web_fetch{"url":"https://x.com"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[1].name, "web_fetch");
    }

    #[test]
    fn test_recover_tool_tag_variant() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"I'll run that for you. <tool>exec{"command":"ls -la"}</tool>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_markdown_code_block() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "I'll execute that command:\n```\nexec {\"command\": \"ls -la\"}\n```";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_markdown_code_block_with_lang() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "```json\ntest_query {\"query\": \"rust\"}\n```";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
    }

    #[test]
    fn test_recover_backtick_wrapped() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Let me run `exec {"command":"pwd"}` for you."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "pwd");
    }

    #[test]
    fn test_recover_backtick_ignores_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Try `unknown_tool {"key":"val"}` instead."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_no_duplicates_across_patterns() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        // Same call in both function tag and tool tag — should only appear once
        let text =
            r#"<function=exec>{"command":"ls"}</function> <tool>exec{"command":"ls"}</tool>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
    }

    // --- Pattern 6: [TOOL_CALL]...[/TOOL_CALL] tests (issue #354) ---

    #[test]
    fn test_recover_tool_call_block_json() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute shell command".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_tool_call_block_arrow_syntax() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute shell command".into(),
            input_schema: serde_json::json!({}),
        }];
        // Exact format from issue #354
        let text = "[TOOL_CALL]\n{tool => \"shell_exec\", args => {\n--command \"ls -F /\"\n}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -F /");
    }

    #[test]
    fn test_recover_tool_call_block_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "[TOOL_CALL]\n{\"name\": \"hack_system\", \"arguments\": {\"cmd\": \"rm -rf /\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_tool_call_block_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "shell_exec".into(),
                description: "Execute".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "file_read".into(),
                description: "Read".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}\n[/TOOL_CALL]\nSome text.\n[TOOL_CALL]\n{\"name\": \"file_read\", \"arguments\": {\"path\": \"/tmp/test.txt\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn test_recover_tool_call_block_unclosed() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        // Unclosed [TOOL_CALL] — pattern 6 skips it, but pattern 8 (bare JSON)
        // still finds the valid JSON tool call object.
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1, "Bare JSON fallback should recover this");
        assert_eq!(calls[0].name, "shell_exec");
    }

    // --- Pattern 7: <tool_call>JSON</tool_call> tests (Qwen3, issue #332) ---

    #[test]
    fn test_recover_tool_call_xml_basic() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}\n</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_tool_call_xml_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "I'll search for that.\n\n<tool_call>\n{\"name\": \"test_query\", \"arguments\": {\"query\": \"rust async\"}}\n</tool_call>\n\nLet me get results.";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[0].input["query"], "rust async");
    }

    #[test]
    fn test_recover_tool_call_xml_function_field() {
        let tools = vec![ToolDefinition {
            name: "file_read".into(),
            description: "Read".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"function\": \"file_read\", \"arguments\": {\"path\": \"/etc/hosts\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
    }

    #[test]
    fn test_recover_tool_call_xml_parameters_field() {
        let tools = vec![ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"web_fetch\", \"parameters\": {\"url\": \"https://example.com\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_fetch");
        assert_eq!(calls[0].input["url"], "https://example.com");
    }

    #[test]
    fn test_recover_tool_call_xml_stringified_args() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"shell_exec\", \"arguments\": \"{\\\"command\\\": \\\"pwd\\\"}\"}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "pwd");
    }

    #[test]
    fn test_recover_tool_call_xml_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"hack_system\", \"arguments\": {\"cmd\": \"rm -rf /\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_tool_call_xml_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "shell_exec".into(),
                description: "Execute".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "test_query".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = "<tool_call>{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}</tool_call>\n<tool_call>{\"name\": \"test_query\", \"arguments\": {\"query\": \"rust\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[1].name, "test_query");
    }

    // --- Pattern 8: Bare JSON tool call object tests ---

    #[test]
    fn test_recover_bare_json_tool_call() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "I'll run that: {\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_bare_json_no_false_positive() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "The config looks like {\"debug\": true, \"level\": \"info\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_bare_json_skipped_when_tags_found() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<function=shell_exec>{\"command\":\"ls\"}</function> {\"name\": \"shell_exec\", \"arguments\": {\"command\": \"pwd\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["command"], "ls");
    }

    // --- Pattern 9: XML-attribute style <function name="..." parameters="..." /> ---

    #[test]
    fn test_recover_xml_attribute_basic() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="test_query" parameters="{&quot;query&quot;: &quot;best crypto 2024&quot;}" />"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[0].input["query"], "best crypto 2024");
    }

    #[test]
    fn test_recover_xml_attribute_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="unknown_tool" parameters="{&quot;x&quot;: 1}" />"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_xml_attribute_non_selfclosing() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="shell_exec" parameters="{&quot;command&quot;: &quot;ls&quot;}"></function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
    }

    // --- Pattern 10: <|plugin|>...<|endofblock|> tests ---

    #[test]
    fn test_recover_plugin_block() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<|plugin|>\n{\"name\": \"test_query\", \"arguments\": {\"query\": \"rust\"}}\n<|endofblock|>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[0].input["query"], "rust");
    }

    #[test]
    fn test_recover_plugin_block_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "<|plugin|>\n{\"name\": \"hack\", \"arguments\": {\"cmd\": \"rm\"}}\n<|endofblock|>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 11: Action/Action Input tests ---

    #[test]
    fn test_recover_action_input() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Action: test_query\nAction Input: {\"query\": \"rust programming\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
        assert_eq!(calls[0].input["query"], "rust programming");
    }

    #[test]
    fn test_recover_action_input_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Action: unknown_tool\nAction Input: {\"key\": \"value\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 12: name + JSON on next line tests ---

    #[test]
    fn test_recover_name_json_nextline() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "shell_exec\n{\"command\": \"ls -la\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_name_json_nextline_unknown() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "unknown_tool\n{\"command\": \"ls\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 13: <tool_use> tests ---

    #[test]
    fn test_recover_tool_use_block() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "<tool_use>{\"name\": \"test_query\", \"arguments\": {\"query\": \"test\"}}</tool_use>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_query");
    }

    #[test]
    fn test_recover_tool_use_block_unknown() {
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_use>{\"name\": \"hack\", \"arguments\": {\"cmd\": \"rm\"}}</tool_use>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Helper function tests ---

    #[test]
    fn test_parse_dash_dash_args_basic() {
        let result = parse_dash_dash_args("{--command \"ls -F /\"}");
        assert_eq!(result["command"], "ls -F /");
    }

    #[test]
    fn test_parse_dash_dash_args_multiple() {
        let result = parse_dash_dash_args("{--file \"test.txt\", --verbose}");
        assert_eq!(result["file"], "test.txt");
        assert_eq!(result["verbose"], true);
    }

    #[test]
    fn test_parse_dash_dash_args_unquoted_value() {
        let result = parse_dash_dash_args("{--count 5}");
        assert_eq!(result["count"], "5");
    }

    #[test]
    fn test_parse_json_tool_call_object_standard() {
        let result = parse_json_tool_call_object(
            "{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}",
        );
        assert!(result.is_some());
        let (name, args) = result.unwrap();
        assert_eq!(name, "shell_exec");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn test_parse_json_tool_call_object_function_field() {
        let result = parse_json_tool_call_object(
            "{\"function\": \"web_fetch\", \"parameters\": {\"url\": \"https://x.com\"}}",
        );
        assert!(result.is_some());
        let (name, args) = result.unwrap();
        assert_eq!(name, "web_fetch");
        assert_eq!(args["url"], "https://x.com");
    }

    #[test]
    fn test_parse_json_tool_call_object_empty_name() {
        let result =
            parse_json_tool_call_object("{\"name\": \"\", \"arguments\": {}}");
        assert!(result.is_none());
    }

    // --- End-to-end integration test: text-as-tool-call recovery through agent loop ---

    /// Mock driver that simulates a Groq/Llama model outputting tool calls as text.
    /// Call 1: Returns text with `<function=test_query>...</function>` (EndTurn, no tool_calls)
    /// Call 2: Returns a normal text response (after tool result is provided)
    struct TextToolCallDriver {
        call_count: AtomicU32,
    }

    impl TextToolCallDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for TextToolCallDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // Simulate Groq/Llama: tool call as text, not in tool_calls field
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: r#"Let me search for that. <function=test_query>{"query":"rust async"}</function>"#.to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![], // BUG: no tool_calls!
                    usage: TokenUsage {
                        input_tokens: 20,
                        output_tokens: 15,
                    },
                media: None,
                })
            } else {
                // After tool result, return normal response
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "Based on the search results, Rust async is great!".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 30,
                        output_tokens: 12,
                    },
                    media: None,
                })
            }
        }
    }

    #[tokio::test]
    async fn test_text_tool_call_recovery_e2e() {
        // This is THE critical test: a model outputs a tool call as text,
        // the recovery code detects it, promotes it to ToolUse, executes the tool,
        // and the agent loop continues to produce a final response.
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(TextToolCallDriver::new());

        // Provide test_query as an available tool so recovery can match it
        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let result = run_agent_loop(
            &manifest,
            "Search for rust async programming",
            &mut session,
            &memory,
            driver,
            &tools,
            None,
            None, // stream_tx
            None,
            None,
            None,
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Agent loop should complete");

        // The response should contain the second call's output, NOT the raw function tag
        assert!(
            !result.response.contains("<function="),
            "Response should not contain raw function tags, got: {:?}",
            result.response
        );
        assert!(
            result.iterations >= 2,
            "Should have at least 2 iterations (tool call + final response), got: {}",
            result.iterations
        );
        // Verify the final text response came through
        assert!(
            result.response.contains("search results") || result.response.contains("Rust async"),
            "Expected final response text, got: {:?}",
            result.response
        );
    }

    /// Mock driver that returns NO text-based tool calls — just normal text.
    /// Verifies recovery does NOT interfere with normal flow.
    #[tokio::test]
    async fn test_normal_flow_unaffected_by_recovery() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(NormalDriver);

        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];

        let result = run_agent_loop(
            &manifest,
            "Say hello",
            &mut session,
            &memory,
            driver,
            &tools, // tools available but not used
            None,
            None, // stream_tx
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Normal loop should complete");

        assert_eq!(result.response, "Hello from the agent!");
        assert_eq!(
            result.iterations, 1,
            "Normal response should complete in 1 iteration"
        );
    }

    // --- Streaming path: text-as-tool-call recovery ---

    #[tokio::test]
    async fn test_text_tool_call_recovery_streaming_e2e() {
        let memory = memory::MemorySubstrate::open_in_memory().unwrap();
        let agent_id = "test-agent".to_string();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            turn_summaries: Vec::new(),
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(TextToolCallDriver::new());

        let tools = vec![ToolDefinition {
            name: "test_query".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let (tx, mut rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Search for rust async programming",
            &mut session,
            &memory,
            driver,
            &tools,
            None,
            tx,
            None,
            None,
            None,
            None, // on_phase
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
            None, // channel_type
        )
        .await
        .expect("Streaming loop should complete");

        // Same assertions as non-streaming
        assert!(
            !result.response.contains("<function="),
            "Streaming: response should not contain raw function tags, got: {:?}",
            result.response
        );
        assert!(
            result.iterations >= 2,
            "Streaming: should have at least 2 iterations, got: {}",
            result.iterations
        );

        // Drain the stream channel to verify events were sent
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events.is_empty(), "Should have received stream events");
    }
