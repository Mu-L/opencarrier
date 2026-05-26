//! Text-based tool call recovery — parse LLM plain-text tool call formats.
//!
//! Many LLMs (Groq/Llama, DeepSeek, Qwen, Ollama) output tool calls as
//! plain text instead of the proper `tool_calls` API field. This module
//! recovers those calls by pattern-matching 13 different text formats.

use types::tool::{ToolCall, ToolDefinition};
use tracing::{info, warn};

/// Result of text-based tool call recovery, including any newly discovered
/// tool definitions that should be added to the agent's tool list.
pub struct RecoveryResult {
    /// Recovered tool calls ready for execution.
    pub calls: Vec<ToolCall>,
    /// Tool definitions discovered via tool_search that were not in the
    /// original available_tools. The caller should add these to the
    /// CompletionRequest.tools list.
    pub discovered_tools: Vec<ToolDefinition>,
    /// Tool names that were referenced in text but could not be recovered
    /// because they had required params missing from the text. The caller
    /// should inject a system message asking the LLM to retry with
    /// structured tool_use.
    pub needs_retry: Vec<String>,
}

/// Callback type for tool_search when a tool name is not in available_tools.
/// Returns an optional ToolDefinition if found.
pub type ToolSearchFn = Box<dyn Fn(&str) -> Option<ToolDefinition> + Send + Sync>;

/// Recover tool calls that LLMs output as plain text instead of the proper
/// `tool_calls` API field. Covers Groq/Llama, DeepSeek, Qwen, and Ollama models.
///
/// Supported patterns:
/// 1. `<function=tool_name>{"key":"value"}</function>`
/// 2. `<function>tool_name{"key":"value"}</function>`
/// 3. `<tool>tool_name{"key":"value"}</tool>`
/// 4. Markdown code blocks containing `tool_name {"key":"value"}`
/// 5. Backtick-wrapped `tool_name {"key":"value"}`
/// 6. `[TOOL_CALL]...[/TOOL_CALL]` blocks (JSON or arrow syntax) — issue #354
/// 7. `üşgûcü{"name":"tool","arguments":{...}}üşgûcü` — Qwen3, issue #332
/// 8. Bare JSON `{"name":"tool","arguments":{...}}` objects (last resort, only if no tags found)
/// 9. `<function name="tool" parameters="{...}" />` — XML attribute style (Groq/Llama)
/// 10. `<|plugin|>...<|endofblock|>` — Qwen/ChatGLM thinking-model format
/// 11. `Action: tool\nAction Input: {"key":"value"}` — ReAct-style (LM Studio, GPT-OSS)
/// 12. `tool_name\n{"key":"value"}` — bare name + JSON on next line (Llama 4 Scout)
/// 13. `<tool_use>{"name":"tool","arguments":{...}}</tool_use>` — Llama 3.1+ variant
///
/// Tool names are validated against `available_tools`. If a tool name is not
/// found and a `tool_search_fn` is provided, it will be called to discover
/// the tool. Discovered tools are included in the result for the caller to
/// add to the tools list, and their required params are checked the same way
/// as known tools.
pub fn recover_text_tool_calls(
    text: &str,
    available_tools: &[ToolDefinition],
    tool_search_fn: Option<ToolSearchFn>,
) -> RecoveryResult {
    let mut calls = Vec::new();
    let mut discovered_tools: Vec<ToolDefinition> = Vec::new();
    let mut needs_retry: Vec<String> = Vec::new();

    let tool_name_set: std::collections::HashSet<&str> =
        available_tools.iter().map(|t| t.name.as_str()).collect();

    // Build a map of tool_name -> required fields from input_schema.
    let mut required_map: std::collections::HashMap<String, Vec<String>> = available_tools
        .iter()
        .filter_map(|t| {
            let required = t.input_schema
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>());
            if let Some(ref req) = required {
                if !req.is_empty() {
                    return Some((t.name.clone(), req.clone()));
                }
            }
            None
        })
        .collect();

    // Pattern 1: <function=TOOL_NAME>JSON_BODY</function>
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find("<function=") {
        let abs_start = search_from + start;
        let after_prefix = abs_start + "<function=".len();

        // Extract tool name (ends at '>')
        let Some(name_end) = text[after_prefix..].find('>') else {
            search_from = after_prefix;
            continue;
        };
        let tool_name = &text[after_prefix..after_prefix + name_end];
        let json_start = after_prefix + name_end + 1;

        // Find closing </function>
        let Some(close_offset) = text[json_start..].find("</function>") else {
            search_from = json_start;
            continue;
        };
        let json_body = text[json_start..json_start + close_offset].trim();
        search_from = json_start + close_offset + "</function>".len();

        // Validate: tool name must be in the current tools list
        if !tool_name_set.contains(tool_name) {
            continue;
        }

        // Parse JSON input
        let input: serde_json::Value = match serde_json::from_str(json_body) {
            Ok(v) => v,
            Err(e) => {
                warn!(tool = tool_name, error = %e, "Failed to parse text-based tool call JSON — skipping");
                continue;
            }
        };

        info!(
            tool = tool_name,
            "Recovered text-based tool call → synthetic ToolUse"
        );
        calls.push(ToolCall {
            id: format!("recovered_{}", uuid::Uuid::new_v4()),
            name: tool_name.to_string(),
            input,
        });
    }

    // Pattern 2: <function>TOOL_NAME{JSON_BODY}</function>
    // (Groq/Llama variant — tool name immediately followed by JSON object)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<function>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<function>".len();

        // Find closing </function>
        let Some(close_offset) = text[after_tag..].find("</function>") else {
            search_from = after_tag;
            continue;
        };
        let inner = &text[after_tag..after_tag + close_offset];
        search_from = after_tag + close_offset + "</function>".len();

        // The inner content is "tool_name{json}" — find the first '{' to split
        let Some(brace_pos) = inner.find('{') else {
            continue;
        };
        let tool_name = inner[..brace_pos].trim();
        let json_body = inner[brace_pos..].trim();

        if tool_name.is_empty() {
            continue;
        }

        // Validate: tool name must be in the current tools list
        if !tool_name_set.contains(tool_name) {
            continue;
        }

        // Parse JSON input
        let input: serde_json::Value = match serde_json::from_str(json_body) {
            Ok(v) => v,
            Err(e) => {
                warn!(tool = tool_name, error = %e, "Failed to parse text-based tool call JSON (variant 2) — skipping");
                continue;
            }
        };

        // Avoid duplicates if pattern 1 already captured this call
        if calls
            .iter()
            .any(|c| c.name == tool_name && c.input == input)
        {
            continue;
        }

        info!(
            tool = tool_name,
            "Recovered text-based tool call (variant 2) → synthetic ToolUse"
        );
        calls.push(ToolCall {
            id: format!("recovered_{}", uuid::Uuid::new_v4()),
            name: tool_name.to_string(),
            input,
        });
    }

    // Pattern 3: <tool>TOOL_NAME{JSON}</tool>  (Qwen / DeepSeek variant)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<tool>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<tool>".len();

        let Some(close_offset) = text[after_tag..].find("</tool>") else {
            search_from = after_tag;
            continue;
        };
        let inner = &text[after_tag..after_tag + close_offset];
        search_from = after_tag + close_offset + "</tool>".len();

        let Some(brace_pos) = inner.find('{') else {
            continue;
        };
        let tool_name = inner[..brace_pos].trim();
        let json_body = inner[brace_pos..].trim();

        if !tool_name_set.contains(tool_name) {
            continue;
        }

        let input: serde_json::Value = match serde_json::from_str(json_body) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if calls
            .iter()
            .any(|c| c.name == tool_name && c.input == input)
        {
            continue;
        }

        info!(
            tool = tool_name,
            "Recovered text-based tool call (<tool> variant) → synthetic ToolUse"
        );
        calls.push(ToolCall {
            id: format!("recovered_{}", uuid::Uuid::new_v4()),
            name: tool_name.to_string(),
            input,
        });
    }

    // Pattern 4: Markdown code blocks containing tool_name {JSON}
    // Matches: ```\nexec {"command":"ls"}\n``` or ```bash\nexec {"command":"ls"}\n```
    {
        let mut in_block = false;
        let mut block_content = String::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                if in_block {
                    // End of block — try to extract tool call from content
                    let content = block_content.trim();
                    if let Some(brace_pos) = content.find('{') {
                        let potential_tool = content[..brace_pos].trim();
                        if tool_name_set.contains(potential_tool) {
                            if let Ok(input) = serde_json::from_str::<serde_json::Value>(
                                content[brace_pos..].trim(),
                            ) {
                                if !calls
                                    .iter()
                                    .any(|c| c.name == potential_tool && c.input == input)
                                {
                                    info!(
                                        tool = potential_tool,
                                        "Recovered tool call from markdown code block"
                                    );
                                    calls.push(ToolCall {
                                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                                        name: potential_tool.to_string(),
                                        input,
                                    });
                                }
                            }
                        }
                    }
                    block_content.clear();
                    in_block = false;
                } else {
                    in_block = true;
                    block_content.clear();
                }
            } else if in_block {
                if !block_content.is_empty() {
                    block_content.push('\n');
                }
                block_content.push_str(trimmed);
            }
        }
    }

    // Pattern 5: Backtick-wrapped tool call: `tool_name {"key":"value"}`
    {
        let parts: Vec<&str> = text.split('`').collect();
        // Every odd-indexed element is inside backticks
        for chunk in parts.iter().skip(1).step_by(2) {
            let trimmed = chunk.trim();
            if let Some(brace_pos) = trimmed.find('{') {
                let potential_tool = trimmed[..brace_pos].trim();
                if tool_name_set.contains(potential_tool) {
                    if let Ok(input) =
                        serde_json::from_str::<serde_json::Value>(trimmed[brace_pos..].trim())
                    {
                        if !calls
                            .iter()
                            .any(|c| c.name == potential_tool && c.input == input)
                        {
                            info!(
                                tool = potential_tool,
                                "Recovered tool call from backtick-wrapped text"
                            );
                            calls.push(ToolCall {
                                id: format!("recovered_{}", uuid::Uuid::new_v4()),
                                name: potential_tool.to_string(),
                                input,
                            });
                        }
                    }
                }
            }
        }
    }

    // Pattern 6: [TOOL_CALL]...[/TOOL_CALL] blocks (Ollama models like Qwen, issue #354)
    // Handles both JSON args and custom `{tool => "name", args => {--key "value"}}` syntax.
    search_from = 0;
    while let Some(start) = text[search_from..].find("[TOOL_CALL]") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "[TOOL_CALL]".len();

        let Some(close_offset) = text[after_tag..].find("[/TOOL_CALL]") else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + "[/TOOL_CALL]".len();

        // Try standard JSON first: {"name":"tool","arguments":{...}}
        if let Some((tool_name, input)) = parse_json_tool_call_object(inner) {
            if !tool_name_set.contains(tool_name.as_str()) {
                continue;
            }
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from [TOOL_CALL] block (JSON)"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
            continue;
        }

        // Custom arrow syntax: {tool => "name", args => {--key "value"}}
        if let Some((tool_name, input)) = parse_arrow_syntax_tool_call(inner) {
            if !tool_name_set.contains(tool_name.as_str()) {
                continue;
            }
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from [TOOL_CALL] block (arrow syntax)"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 7: üşgûcüJSONüşgûcü (Qwen3 models on Ollama, issue #332)
    search_from = 0;
    while let Some(start) = text[search_from..].find("üşgûcü") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "üşgûcü".len();

        let Some(close_offset) = text[after_tag..].find("üşgûcü") else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + "üşgûcü".len();

        if let Some((tool_name, input)) = parse_json_tool_call_object(inner) {
            if !tool_name_set.contains(tool_name.as_str()) {
                continue;
            }
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from üşgûcü block"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 9: <function name="tool" parameters="{...}" /> — XML attribute style
    // Groq/Llama sometimes emit self-closing XML with name/parameters attributes.
    // The parameters value is HTML-entity-escaped JSON (&quot; etc.).
    {
        use regex_lite::Regex;
        // Match both self-closing <function ... /> and <function ...></function>
        let re =
            Regex::new(r#"<function\s+name="([^"]+)"\s+parameters="([^"]*)"[^/]*/?>"#).unwrap();
        for caps in re.captures_iter(text) {
            let tool_name = caps.get(1).unwrap().as_str();
            let raw_params = caps.get(2).unwrap().as_str();

            if !tool_name_set.contains(tool_name) {
                continue;
            }

            // Unescape HTML entities (&quot; &amp; &lt; &gt; &apos;)
            let unescaped = raw_params
                .replace("&quot;", "\"")
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&apos;", "'");

            let input: serde_json::Value = match serde_json::from_str(&unescaped) {
                Ok(v) => v,
                Err(e) => {
                    warn!(tool = tool_name, error = %e, "Failed to parse XML-attribute tool call params — skipping");
                    continue;
                }
            };

            if calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                continue;
            }

            info!(
                tool = tool_name,
                "Recovered XML-attribute tool call → synthetic ToolUse"
            );
            calls.push(ToolCall {
                id: format!("recovered_{}", uuid::Uuid::new_v4()),
                name: tool_name.to_string(),
                input,
            });
        }
    }

    // Pattern 10: <|plugin|>...<|endofblock|> (Qwen/ChatGLM thinking-model format)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<|plugin|>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<|plugin|>".len();

        let close_tag = "<|endofblock|>";
        let Some(close_offset) = text[after_tag..].find(close_tag) else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + close_tag.len();

        if let Some((tool_name, input)) = parse_json_tool_call_object(inner) {
            if !tool_name_set.contains(tool_name.as_str()) {
                continue;
            }
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from <|plugin|> block"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 11: Action: tool_name\nAction Input: {JSON} (ReAct-style, LM Studio / GPT-OSS)
    {
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i].trim();
            if let Some(tool_part) = line
                .strip_prefix("Action:")
                .or_else(|| line.strip_prefix("action:"))
            {
                let tool_name = tool_part.trim();
                if tool_name_set.contains(tool_name) {
                    // Look for "Action Input:" on the next line(s)
                    if i + 1 < lines.len() {
                        let next = lines[i + 1].trim();
                        if let Some(json_part) = next
                            .strip_prefix("Action Input:")
                            .or_else(|| next.strip_prefix("action input:"))
                            .or_else(|| next.strip_prefix("action_input:"))
                        {
                            let json_str = json_part.trim();
                            if let Ok(input) = serde_json::from_str::<serde_json::Value>(json_str) {
                                if !calls
                                    .iter()
                                    .any(|c| c.name == tool_name && c.input == input)
                                {
                                    info!(
                                        tool = tool_name,
                                        "Recovered tool call from Action/Action Input pattern"
                                    );
                                    calls.push(ToolCall {
                                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                                        name: tool_name.to_string(),
                                        input,
                                    });
                                }
                            }
                            i += 2;
                            continue;
                        }
                    }
                }
            }
            i += 1;
        }
    }

    // Pattern 12: tool_name\n{"key":"value"} — bare name + JSON on next line (Llama 4 Scout)
    {
        let lines: Vec<&str> = text.lines().collect();
        for i in 0..lines.len().saturating_sub(1) {
            let name_line = lines[i].trim();
            // Tool name must be a single word in the current tools list
            if !tool_name_set.contains(name_line) {
                continue;
            }
            // Next line must be valid JSON
            let json_line = lines[i + 1].trim();
            if !json_line.starts_with('{') {
                continue;
            }
            if let Ok(input) = serde_json::from_str::<serde_json::Value>(json_line) {
                if !calls
                    .iter()
                    .any(|c| c.name == name_line && c.input == input)
                {
                    info!(
                        tool = name_line,
                        "Recovered tool call from name+JSON line pair"
                    );
                    calls.push(ToolCall {
                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                        name: name_line.to_string(),
                        input,
                    });
                }
            }
        }
    }

    // Pattern 13: <tool_use>JSON</tool_use> (Llama 3.1+ variant)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<tool_use>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<tool_use>".len();

        let Some(close_offset) = text[after_tag..].find("</tool_use>") else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + "</tool_use>".len();

        if let Some((tool_name, input)) = parse_json_tool_call_object(inner) {
            if !tool_name_set.contains(tool_name.as_str()) {
                continue;
            }
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from <tool_use> block"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 8: Bare JSON tool call objects in text (common Ollama fallback)
    // Matches: {"name":"tool_name","arguments":{"key":"value"}} not already inside tags
    // Only try this if no calls were found by tag-based patterns, to avoid false positives.
    if calls.is_empty() {
        // Scan for JSON objects that look like tool calls
        let mut scan_from = 0;
        while let Some(brace_start) = text[scan_from..].find('{') {
            let abs_brace = scan_from + brace_start;
            // Try to parse a JSON object starting here
            if let Some((tool_name, input)) =
                try_parse_bare_json_tool_call(&text[abs_brace..], &[])
            {
                if !tool_name_set.contains(tool_name.as_str()) {
                    scan_from = abs_brace + 1;
                    continue;
                }
                if !calls
                    .iter()
                    .any(|c| c.name == tool_name && c.input == input)
                {
                    info!(
                        tool = tool_name.as_str(),
                        "Recovered tool call from bare JSON object in text"
                    );
                    calls.push(ToolCall {
                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                        name: tool_name,
                        input,
                    });
                }
            }
            scan_from = abs_brace + 1;
        }
    }

    // Pattern 14: [Called tool_name] — LLM text description of a tool call.
    // For tools in available_tools: recover directly.
    // For tools NOT in available_tools: try tool_search_fn to discover them.
    // Matches [Called ...] anywhere in the text, not just at line start.
    {
        let mut search_from = 0;
        while let Some(pos) = text[search_from..].find("[Called ") {
            let after = &text[search_from + pos + "[Called ".len()..];
            let Some(close) = after.find(']') else {
                search_from += pos + "[Called ".len();
                continue;
            };
            let inner = &after[..close];
            let tool_name = inner
                .find(|c: char| c == ' ' || c == ':' || c == '(' || c == '{')
                .map(|pos| &inner[..pos])
                .unwrap_or(inner);

            if tool_name.is_empty() || tool_name.contains(' ') {
                search_from += pos + "[Called ".len();
                continue;
            }

            // Skip if already recovered by an earlier pattern
            if calls.iter().any(|c| c.name == tool_name) {
                search_from += pos + "[Called ".len();
                continue;
            }

            let input = if let Some(json_start) = inner.find('{') {
                if let Some(json_end) = inner.rfind('}') {
                    serde_json::from_str::<serde_json::Value>(&inner[json_start..=json_end])
                        .unwrap_or(serde_json::json!({}))
                } else {
                    serde_json::json!({})
                }
            } else {
                serde_json::json!({})
            };

            if tool_name_set.contains(tool_name) {
                info!(
                    tool = tool_name,
                    "Recovered tool call from [Called ...] pattern"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name.to_string(),
                    input,
                });
            } else if let Some(ref search_fn) = tool_search_fn {
                // Tool not in available_tools — try tool_search
                if let Some(def) = search_fn(tool_name) {
                    info!(
                        tool = tool_name,
                        "Discovered tool via tool_search from [Called ...] pattern"
                    );
                    // Add to required_map for unified param filtering
                    if let Some(required) = def.input_schema
                        .get("required")
                        .and_then(|r| r.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
                    {
                        if !required.is_empty() {
                            required_map.insert(def.name.clone(), required);
                        }
                    }
                    discovered_tools.push(def);
                    calls.push(ToolCall {
                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                        name: tool_name.to_string(),
                        input,
                    });
                }
            }
            // Advance past this match to avoid infinite loop
            search_from += pos + "[Called ".len() + close + 1;
        }
    }

    // Unified required-params filtering for all recovered calls.
    // Calls missing required params are removed; their tool names are collected
    // into needs_retry so the caller can inject a system message asking the LLM
    // to retry with structured tool_use.
    let mut filtered_calls = Vec::new();
    for call in calls {
        if let Some(required) = required_map.get(&call.name) {
            if !required.is_empty() {
                let input_is_empty = call.input.as_object()
                    .map(|obj| obj.is_empty())
                    .unwrap_or(true);
                if input_is_empty {
                    info!(
                        tool = %call.name,
                        required = ?required,
                        "Skipping text-recovered tool call: input is empty but tool has required params"
                    );
                    if !needs_retry.iter().any(|n| n == &call.name) {
                        needs_retry.push(call.name.clone());
                    }
                    continue;
                }
                let input_obj = call.input.as_object();
                let missing: Vec<&str> = required.iter()
                    .filter(|r| input_obj.map_or(true, |o| !o.contains_key(r.as_str())))
                    .map(|s| s.as_str())
                    .collect();
                if !missing.is_empty() {
                    info!(
                        tool = %call.name,
                        missing = ?missing,
                        "Skipping text-recovered tool call: missing required params"
                    );
                    if !needs_retry.iter().any(|n| n == &call.name) {
                        needs_retry.push(call.name.clone());
                    }
                    continue;
                }
            }
        }
        filtered_calls.push(call);
    }

    RecoveryResult {
        calls: filtered_calls,
        discovered_tools,
        needs_retry,
    }
}

/// Parse a JSON object that represents a tool call.
/// Supports formats:
/// - `{"name":"tool","arguments":{"key":"value"}}`
/// - `{"name":"tool","parameters":{"key":"value"}}`
/// - `{"function":"tool","arguments":{"key":"value"}}`
/// - `{"tool":"tool_name","args":{"key":"value"}}`
pub(crate) fn parse_json_tool_call_object(
    text: &str,
) -> Option<(String, serde_json::Value)> {
    let obj: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = obj.as_object()?;

    // Extract tool name from various field names
    let name = obj
        .get("name")
        .or_else(|| obj.get("function"))
        .or_else(|| obj.get("tool"))
        .and_then(|v| v.as_str())?;

    if name.is_empty() || name.contains(' ') {
        return None;
    }

    // Extract arguments from various field names
    let args = obj
        .get("arguments")
        .or_else(|| obj.get("parameters"))
        .or_else(|| obj.get("args"))
        .or_else(|| obj.get("input"))
        .cloned()
        .unwrap_or(serde_json::json!({}));

    // If arguments is a string (some models stringify it), try to parse it
    let args = if let Some(s) = args.as_str() {
        serde_json::from_str(s).unwrap_or(serde_json::json!({}))
    } else {
        args
    };

    Some((name.to_string(), args))
}

/// Parse the custom arrow syntax used by some Ollama models:
/// `{tool => "name", args => {--key "value"}}` or `{tool => "name", args => {"key":"value"}}`
pub(crate) fn parse_arrow_syntax_tool_call(
    text: &str,
) -> Option<(String, serde_json::Value)> {
    // Extract tool name: look for `tool => "name"` or `tool=>"name"`
    let tool_marker_pos = text.find("tool")?;
    let after_tool = &text[tool_marker_pos + 4..];
    // Skip whitespace and `=>`
    let after_arrow = after_tool.trim_start();
    let after_arrow = after_arrow.strip_prefix("=>")?;
    let after_arrow = after_arrow.trim_start();

    // Extract quoted tool name
    let tool_name = if let Some(stripped) = after_arrow.strip_prefix('"') {
        let end_quote = stripped.find('"')?;
        &stripped[..end_quote]
    } else {
        // Unquoted: take until comma, whitespace, or '}'
        let end = after_arrow
            .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
            .unwrap_or(after_arrow.len());
        &after_arrow[..end]
    };

    if tool_name.is_empty() || tool_name.contains(' ') {
        return None;
    }

    // Extract args: look for `args => {` or `args=>{`
    let args_value = if let Some(args_pos) = text.find("args") {
        let after_args = &text[args_pos + 4..];
        let after_args = after_args.trim_start();
        let after_args = after_args.strip_prefix("=>")?;
        let after_args = after_args.trim_start();

        if after_args.starts_with('{') {
            // Try standard JSON parse first
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(after_args) {
                v
            } else {
                // Parse `--key "value"` / `--key value` style args
                parse_dash_dash_args(after_args)
            }
        } else {
            serde_json::json!({})
        }
    } else {
        serde_json::json!({})
    };

    Some((tool_name.to_string(), args_value))
}

/// Parse `{--key "value", --flag}` or `{--command "ls -F /"}` style arguments
/// into a JSON object.
pub(crate) fn parse_dash_dash_args(text: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();

    // Strip outer braces — find matching close brace
    let inner = if text.starts_with('{') {
        let mut depth = 0;
        let mut end = text.len();
        for (i, c) in text.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        text[1..end].trim()
    } else {
        text.trim()
    };

    // Parse --key "value" or --key value pairs
    let mut remaining = inner;
    while let Some(dash_pos) = remaining.find("--") {
        remaining = &remaining[dash_pos + 2..];

        // Extract key: runs until whitespace, '=', '"', or end
        let key_end = remaining
            .find(|c: char| c.is_whitespace() || c == '=' || c == '"')
            .unwrap_or(remaining.len());
        let key = &remaining[..key_end];
        if key.is_empty() {
            continue;
        }
        remaining = &remaining[key_end..];
        remaining = remaining.trim_start();

        // Skip optional '='
        if remaining.starts_with('=') {
            remaining = remaining[1..].trim_start();
        }

        // Extract value
        if remaining.starts_with('"') {
            // Quoted value — find closing quote
            if let Some(end_quote) = remaining[1..].find('"') {
                let value = &remaining[1..1 + end_quote];
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
                remaining = &remaining[2 + end_quote..];
            } else {
                // Unclosed quote — take rest
                let value = &remaining[1..];
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
                break;
            }
        } else {
            // Unquoted value — take until next --, comma, }, or end
            let val_end = remaining
                .find([',', '}'])
                .or_else(|| remaining.find("--"))
                .unwrap_or(remaining.len());
            let value = remaining[..val_end].trim();
            if !value.is_empty() {
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            } else {
                // Flag with no value — set to true
                map.insert(key.to_string(), serde_json::Value::Bool(true));
            }
            remaining = &remaining[val_end..];
        }

        // Skip comma separator
        remaining = remaining.trim_start();
        if remaining.starts_with(',') {
            remaining = remaining[1..].trim_start();
        }
    }

    serde_json::Value::Object(map)
}

/// Try to parse a bare JSON object as a tool call.
/// The JSON must have a "name"/"function"/"tool" field matching a known tool.
pub(crate) fn try_parse_bare_json_tool_call(
    text: &str,
    _tool_names: &[&str],
) -> Option<(String, serde_json::Value)> {
    // Find the end of this JSON object by counting braces
    let mut depth = 0;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        return None;
    }

    parse_json_tool_call_object(&text[..end])
}

/// Strip `[Called ...]` patterns from response text so users never see
/// raw tool-call syntax, even when text-based recovery gave up.
pub fn strip_tool_call_artifacts(text: &str) -> String {
    let mut result = text.to_string();
    let mut search_from = 0;
    while let Some(pos) = result[search_from..].find("[Called ") {
        let abs_pos = search_from + pos;
        let after = &result[abs_pos + "[Called ".len()..];
        if let Some(close) = after.find(']') {
            result.replace_range(abs_pos..abs_pos + "[Called ".len() + close + 1, "");
            // Don't advance — re-scan from same position since text shifted
            search_from = abs_pos;
        } else {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests_strip {
    use super::*;

    #[test]
    fn test_strip_single_called() {
        assert_eq!(
            strip_tool_call_artifacts("还没，正在执行排版和发布流程。[Called knowledge_read]"),
            "还没，正在执行排版和发布流程。"
        );
    }

    #[test]
    fn test_strip_multiple_called() {
        assert_eq!(
            strip_tool_call_artifacts("我需要先搜索一下。[Called tool_search] 然后再读。[Called knowledge_read]"),
            "我需要先搜索一下。 然后再读。"
        );
    }

    #[test]
    fn test_strip_no_called() {
        assert_eq!(
            strip_tool_call_artifacts("这是一条普通回复，没有工具调用。"),
            "这是一条普通回复，没有工具调用。"
        );
    }

    #[test]
    fn test_strip_unclosed_bracket_ignored() {
        assert_eq!(
            strip_tool_call_artifacts("这里有个 [Called tool 没有闭合"),
            "这里有个 [Called tool 没有闭合"
        );
    }
}
