//! Flow definition types and frontmatter parsing.
//!
//! A flow is the capability unit (replaces the legacy "skill"). It has two forms:
//! - **single-step** (`steps` empty/absent): body injected into the system prompt,
//!   the LLM runs freely in `run_agent_loop` (equivalent to the legacy skill).
//! - **multi-step** (`steps` non-empty): a DAG executed by `run_flow`.
//!
//! This module is the single authoritative parser for flow frontmatter. It lives
//! in `types` (not `kernel`) so both `kernel` (classification) and `runtime`
//! (`run_flow` execution) can share it without violating the `kernel -> runtime`
//! dependency direction.

use serde_json::{Map, Value};

/// How a single step is executed. `AgentLoop`, `Chat`, `UserInput`,
/// `FlowExec`, and `Map` are executed by `run_flow`; `Tool` is parsed but not
/// yet executed (rejected as unsupported); other kinds are preserved as
/// `Unknown` so later stages can add execution without touching the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    AgentLoop,
    Chat,
    Tool,
    /// Suspend the flow and ask the human a question; resume on their next
    /// message (stage D). The user's reply becomes this step's output
    /// `{ decision, text }`.
    UserInput,
    /// Invoke another flow by name (stage E.1). `with` becomes the sub-flow's
    /// `input`; output is the sub-flow's final value.
    FlowExec,
    /// Iterate a dynamic array (`over`), running a sub-flow per element (stage
    /// E.1, serial batch). Output is the collected results array.
    Map,
    /// A recognized-but-not-yet-executed kind (e.g. `delegate`). `run_flow`
    /// rejects these until later stages.
    Unknown(String),
}

impl StepKind {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "agent_loop" => Self::AgentLoop,
            "chat" => Self::Chat,
            "tool" => Self::Tool,
            "user_input" => Self::UserInput,
            "flow_exec" => Self::FlowExec,
            "map" => Self::Map,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// True if `run_flow` can currently execute this kind. All of
    /// `AgentLoop`/`Chat`/`UserInput`/`FlowExec`/`Map`/`Tool` are executed;
    /// other kinds are preserved as `Unknown` so later stages can add
    /// execution without touching the parser.
    pub fn is_executable(&self) -> bool {
        matches!(
            self,
            Self::AgentLoop | Self::Chat | Self::UserInput | Self::FlowExec | Self::Map
                | Self::Tool
        )
    }
}

/// How a step's output is captured (see refactor doc §4.5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StepOutputMode {
    /// LLM final message text (default).
    #[default]
    Llm,
    /// Read a file's content at step completion. Path may be templated.
    File(String),
    /// Parse the LLM final message as JSON.
    Json,
}

impl StepOutputMode {
    /// Parse a raw `output:` frontmatter value.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s == "json" {
            Self::Json
        } else if let Some(p) = s.strip_prefix("file:") {
            Self::File(p.trim().to_string())
        } else {
            Self::Llm // "llm" or anything unrecognized -> default
        }
    }
}

/// A single step in a multi-step flow DAG.
#[derive(Debug, Clone, Default)]
pub struct StepDef {
    /// Step identifier (required, unique within a flow).
    pub id: String,
    /// Execution kind. `None` means `kind:` was absent (invalid).
    pub kind: Option<StepKind>,
    /// IDs of steps that must complete before this one (DAG edges).
    pub depends_on: Vec<String>,
    /// Condition expression evaluated before execution, e.g.
    /// `"review.decision == 'revise'"`. `false` -> step is skipped.
    pub when: Option<String>,
    /// Step to jump to on failure (graceful degradation).
    pub on_failure: Option<String>,
    /// Raw `output:` value; resolved to [`StepOutputMode`] at execution time.
    pub output: Option<String>,
    /// Step-specific instruction prompt (may contain templates).
    pub prompt: Option<String>,
    /// Task text for `chat` steps.
    pub task: Option<String>,
    /// Tool name for `tool` steps.
    pub tool_name: Option<String>,
    /// Tool arguments for `tool` steps (template strings allowed in values).
    pub tool_args: Value,
    /// Parameters passed to the step (template strings as values).
    pub with: Map<String, Value>,
    /// Cancel keywords for `user_input` steps (case-insensitive substring
    /// match against the user's reply -> `decision = "cancel"`).
    pub cancel_keywords: Vec<String>,
    /// Per-step timeout for `user_input` steps, in hours. `None` => the
    /// kernel config default (`user_input_timeout_secs`).
    pub timeout_hours: Option<f64>,
    /// Sub-flow name for `flow_exec` steps and `map` step bodies (stage E.1).
    pub flow: Option<String>,
    /// Template resolving to a JSON array, iterated by `map` steps.
    pub over: Option<String>,
    /// Element binding name in `map` step templates (defaults to `"item"`).
    pub as_name: Option<String>,
    /// Inline steps body for interactive `map` steps (stage E.2). When set, the
    /// map iterates `over` running this step list per element (may contain
    /// `user_input`, suspending via `map_context`); when `None`, the map uses
    /// `flow`/`with` (batch form, stage E.1).
    pub body: Option<Vec<StepDef>>,
    /// Concurrency for batch `map` steps (stage E.1): up to this many sub-flows
    /// run at once. `None`/`1` => serial (default). Ignored (must be 1) for
    /// interactive maps (`body` set), which can suspend per element.
    pub parallel: Option<u32>,
}

impl StepDef {
    /// Resolved output mode (defaults to [`StepOutputMode::Llm`]).
    pub fn output_mode(&self) -> StepOutputMode {
        self.output.as_deref().map(StepOutputMode::parse).unwrap_or_default()
    }
}

/// A parsed flow definition.
#[derive(Debug, Clone, Default)]
pub struct FlowDef {
    pub name: String,
    pub description: String,
    pub max_iterations: Option<u32>,
    pub tools: Vec<String>,
    /// Instruction body (markdown after frontmatter).
    pub body: String,
    /// Steps. Empty => single-step flow.
    pub steps: Vec<StepDef>,
    /// Which step's output is the flow's final result. Defaults to the last
    /// executed (non-skipped) step.
    pub final_step: Option<String>,
    /// `false` => not selectable by `classify_flow` (pure atomic). Defaults to true.
    pub entry: Option<bool>,
    /// Top-level `output` for single-step flows.
    pub output: Option<String>,
}

impl FlowDef {
    /// True if this is a multi-step (DAG) flow.
    pub fn is_multi_step(&self) -> bool {
        !self.steps.is_empty()
    }
}

/// Parse a flow `.md` file's content into a [`FlowDef`].
///
/// Frontmatter is YAML-like (delimited by `---`); the body is everything after
/// the closing `---`. Unknown frontmatter keys are ignored. The `steps:` block
/// is parsed as a nested list-of-maps (a constrained YAML subset).
pub fn parse_flow_def(content: &str) -> FlowDef {
    let content = content.trim();
    let (frontmatter, body) = split_frontmatter(content);

    let mut def = FlowDef {
        body: body.to_string(),
        ..Default::default()
    };
    if frontmatter.is_empty() {
        return def;
    }

    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        // Only top-level (indent 0) keys are handled here; nested content under
        // `steps:` is consumed wholesale by `parse_steps_block`.
        let indent = line.len() - line.trim_start().len();
        if indent != 0 {
            i += 1;
            continue;
        }
        let trimmed = line.trim();

        if let Some(val) = trimmed.strip_prefix("name:") {
            def.name = unquote(val);
            i += 1;
        } else if let Some(val) = trimmed.strip_prefix("description:") {
            def.description = unquote(val);
            i += 1;
        } else if let Some(val) = trimmed.strip_prefix("max_iterations:") {
            def.max_iterations = unquote(val).parse().ok();
            i += 1;
        } else if let Some(val) = trimmed.strip_prefix("final:") {
            let s = unquote(val);
            def.final_step = (!s.is_empty()).then_some(s);
            i += 1;
        } else if let Some(val) = trimmed.strip_prefix("entry:") {
            def.entry = match unquote(val).as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            i += 1;
        } else if let Some(val) = trimmed.strip_prefix("output:") {
            let s = unquote(val);
            def.output = (!s.is_empty()).then_some(s);
            i += 1;
        } else if trimmed == "tools:" || trimmed.starts_with("tools:") {
            let (list, consumed) = parse_top_array(&lines, i);
            def.tools = list;
            i += consumed.max(1);
        } else if trimmed == "steps:" || trimmed.starts_with("steps:") {
            let inline = trimmed.strip_prefix("steps:").unwrap_or("").trim();
            if inline == "[]" {
                def.steps = Vec::new();
                i += 1;
            } else if inline.is_empty() {
                let (steps, consumed) = parse_steps_block(&lines, i);
                def.steps = steps;
                i += consumed.max(1);
            } else {
                // Inline steps (e.g. `steps: [...]`) are not supported; ignore.
                i += 1;
            }
        } else {
            // Unknown top-level key (e.g. `version:`) - skip.
            i += 1;
        }
    }

    def
}

/// Split `---\n<fm>\n---\n<body>` into (frontmatter, body). If there is no
/// frontmatter, returns ("", content).
fn split_frontmatter(content: &str) -> (&str, &str) {
    let rest = match content.strip_prefix("---") {
        Some(r) => r,
        None => return ("", content),
    };
    match rest.find("---") {
        Some(end) => (&rest[..end], rest[end + 3..].trim()),
        None => ("", content),
    }
}

/// Parse a top-level array field (inline `[a, b]` or block `  - a` form).
/// Returns (values, lines_consumed_including_the_key_line).
fn parse_top_array(lines: &[&str], key_idx: usize) -> (Vec<String>, usize) {
    let inline = lines[key_idx]
        .trim()
        .strip_prefix("tools:")
        .unwrap_or("")
        .trim();
    if !inline.is_empty() {
        // inline form (also handles `[]`)
        return (parse_inline_list(inline), 1);
    }
    // block form: collect subsequent `  - x` lines at indent > 0
    let mut out = Vec::new();
    let mut j = key_idx + 1;
    while j < lines.len() {
        let l = lines[j];
        if l.trim().is_empty() {
            j += 1;
            continue;
        }
        let indent = l.len() - l.trim_start().len();
        if indent == 0 {
            break;
        }
        let t = l.trim_start();
        if let Some(item) = t.strip_prefix('-') {
            let v = unquote(item.trim());
            if !v.is_empty() {
                out.push(v);
            }
            j += 1;
        } else {
            // a non-list indented line ends the block
            break;
        }
    }
    (out, j - key_idx)
}

/// Parse the nested `steps:` block into step definitions.
/// `start` is the index of the `steps:` line.
fn parse_steps_block(lines: &[&str], start: usize) -> (Vec<StepDef>, usize) {
    // Gather block lines (indent > 0) following `steps:`.
    let mut block: Vec<(usize, String)> = Vec::new();
    let mut j = start + 1;
    while j < lines.len() {
        let raw = lines[j];
        if raw.trim().is_empty() {
            j += 1;
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        if indent == 0 {
            break;
        }
        block.push((indent, raw.trim_start().to_string()));
        j += 1;
    }
    let consumed = j - start;
    if block.is_empty() {
        return (Vec::new(), consumed);
    }

    (parse_step_list(&block), consumed)
}

/// Parse a list of step definitions from a block of `(indent, text)` lines.
/// Self-contained: computes its own `item_indent`/`field_indent` from the
/// block, so it can be called recursively for a nested `body:` step list.
fn parse_step_list(block: &[(usize, String)]) -> Vec<StepDef> {
    if block.is_empty() {
        return Vec::new();
    }

    // Items begin with `- ` at the minimum indent among dash-lines.
    let item_indent = block
        .iter()
        .filter(|(_, t)| t.starts_with('-'))
        .map(|(ind, _)| *ind)
        .min()
        .unwrap_or(2);
    let field_indent = item_indent + 2; // fields align after "- "

    let mut steps: Vec<StepDef> = Vec::new();
    // When collecting a block field (depends_on list / with map / body step
    // list), holds its name.
    let mut pending: Option<String> = None;
    // Buffered lines for a nested `body:` step list (indent > field_indent).
    let mut body_buf: Vec<(usize, String)> = Vec::new();

    /// Flush a completed `body:` sub-block onto the current step.
    fn flush_body(steps: &mut [StepDef], body_buf: &mut Vec<(usize, String)>) {
        if let Some(s) = steps.last_mut() {
            if !body_buf.is_empty() {
                s.body = Some(parse_step_list(body_buf));
            }
        }
        body_buf.clear();
    }

    for (indent, text) in block {
        let t = text.as_str();
        let is_dash = t.starts_with('-');

        // While collecting a `body:` step list, absorb deeper-indented lines
        // until we drop back to field_indent or shallower.
        if pending.as_deref() == Some("body") {
            if *indent > field_indent {
                body_buf.push((*indent, t.to_string()));
                continue;
            }
            // Left the body block: flush it, then process this line normally.
            flush_body(&mut steps, &mut body_buf);
            pending = None;
        }

        // New step item: `  - id: draft`
        if *indent == item_indent && is_dash {
            pending = None;
            let mut s = StepDef::default();
            let first = t.strip_prefix('-').unwrap_or(t).trim();
            if !first.is_empty() {
                pending = apply_step_field(&mut s, first);
            }
            steps.push(s);
            continue;
        }

        let Some(s) = steps.last_mut() else { continue };

        // Continuation of a pending block field.
        match pending.as_deref() {
            Some("depends_on") if is_dash => {
                let v = unquote(t.strip_prefix('-').unwrap_or(t).trim());
                if !v.is_empty() {
                    s.depends_on.push(v);
                }
                continue;
            }
            Some("cancel_keywords") if is_dash => {
                let v = unquote(t.strip_prefix('-').unwrap_or(t).trim());
                if !v.is_empty() {
                    s.cancel_keywords.push(v);
                }
                continue;
            }
            Some("with") if !is_dash && *indent > field_indent => {
                let (k, v) = split_kv(t);
                if !k.is_empty() {
                    s.with.insert(k, Value::String(unquote(&v)));
                }
                continue;
            }
            _ => {}
        }

        // A field line at field_indent (ends any pending block).
        if !is_dash && *indent == field_indent {
            pending = apply_step_field(s, t);
        }
        // Anything else (deeper non-matching content) is ignored.
    }

    // Flush a `body:` block left open at end of input.
    if pending.as_deref() == Some("body") {
        flush_body(&mut steps, &mut body_buf);
    }

    steps
}

/// Apply a single `key: value` field to a step. Returns `Some(field_name)` when
/// the value is empty and the field opens a block (depends_on / with) that the
/// caller should collect from subsequent lines.
fn apply_step_field(s: &mut StepDef, text: &str) -> Option<String> {
    let (k, v) = split_kv(text);
    let v = unquote(&v);
    match k.as_str() {
        "id" => s.id = v,
        "kind" => s.kind = Some(StepKind::parse(&v)),
        "depends_on" => {
            if v.is_empty() {
                return Some("depends_on".into());
            }
            s.depends_on = parse_inline_list(&v);
        }
        "when" => s.when = (!v.is_empty()).then_some(v),
        "on_failure" => s.on_failure = (!v.is_empty()).then_some(v),
        "output" => s.output = (!v.is_empty()).then_some(v),
        "prompt" => s.prompt = (!v.is_empty()).then_some(v),
        "task" => s.task = (!v.is_empty()).then_some(v),
        "tool" | "tool_name" => s.tool_name = (!v.is_empty()).then_some(v),
        "tool_args" => {
            if !v.is_empty() {
                s.tool_args = parse_value(&v);
            }
        }
        "with" => {
            if v.is_empty() {
                return Some("with".into());
            }
            s.with = parse_inline_map(&v);
        }
        "cancel_keywords" => {
            if v.is_empty() {
                return Some("cancel_keywords".into());
            }
            s.cancel_keywords = parse_inline_list(&v);
        }
        "timeout_hours" => s.timeout_hours = (!v.is_empty()).then(|| v.parse().ok()).flatten(),
        "flow" => s.flow = (!v.is_empty()).then_some(v),
        "over" => s.over = (!v.is_empty()).then_some(v),
        "as" => s.as_name = (!v.is_empty()).then_some(v),
        "parallel" => s.parallel = (!v.is_empty()).then(|| v.parse().ok()).flatten(),
        "body" => {
            // Block form (`body:` on its own line) opens a nested step list
            // collected by `parse_step_list`; inline form is unsupported.
            if v.is_empty() {
                return Some("body".into());
            }
        }
        _ => {}
    }
    None
}

/// Split `key: value` (value may be empty). Trims both sides; value keeps its
/// raw form (quotes stripped later by [`unquote`]).
fn split_kv(text: &str) -> (String, String) {
    match text.split_once(':') {
        Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
        None => (text.trim().to_string(), String::new()),
    }
}

/// Trim and strip surrounding single/double quotes.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(s);
    let s = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).unwrap_or(s);
    s.trim().to_string()
}

/// Parse an inline list `[a, b, "c"]` (also tolerates a bare `a, b`).
fn parse_inline_list(s: &str) -> Vec<String> {
    let s = s.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(s);
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(unquote)
        .filter(|x| !x.is_empty())
        .collect()
}

/// Parse an inline map `{k: v, k2: v2}` (values become JSON strings).
fn parse_inline_map(s: &str) -> Map<String, Value> {
    let s = s.trim();
    let inner = s
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .unwrap_or(s);
    let mut m = Map::new();
    if inner.trim().is_empty() {
        return m;
    }
    for pair in inner.split(',') {
        if let Some((k, v)) = pair.split_once(':') {
            let k = unquote(k);
            if !k.is_empty() {
                m.insert(k, Value::String(unquote(v)));
            }
        }
    }
    m
}

/// Parse a value that may be an inline map (`{k: v}`), inline list (`[a, b]`),
/// or a bare string. Keys/values are YAML-ish (quotes optional); values become
/// JSON strings, sufficient for template-string step args.
fn parse_value(s: &str) -> Value {
    let s = s.trim();
    if s.starts_with('{') {
        Value::Object(parse_inline_map(s))
    } else if s.starts_with('[') {
        Value::Array(
            parse_inline_list(s)
                .into_iter()
                .map(Value::String)
                .collect(),
        )
    } else {
        Value::String(unquote(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_step_no_steps() {
        let content = r#"---
name: analyze
description: 定居点分析
tools: ["file_read", "web_search"]
---
# Analyze
Do the thing."#;
        let f = parse_flow_def(content);
        assert_eq!(f.name, "analyze");
        assert_eq!(f.description, "定居点分析");
        assert_eq!(f.tools, vec!["file_read", "web_search"]);
        assert!(f.steps.is_empty());
        assert!(!f.is_multi_step());
        assert_eq!(f.body, "# Analyze\nDo the thing.");
    }

    #[test]
    fn multiline_tools_block() {
        let content = r#"---
name: t
description: d
tools:
  - web_search
  - knowledge_add
---
body"#;
        let f = parse_flow_def(content);
        assert_eq!(f.tools, vec!["web_search", "knowledge_add"]);
    }

    #[test]
    fn tools_block_stops_at_next_key() {
        let content = r#"---
name: t
description: d
tools:
  - foo
  - bar
version: 2
---
body"#;
        let f = parse_flow_def(content);
        assert_eq!(f.tools, vec!["foo", "bar"]);
    }

    #[test]
    fn multi_step_dag_basic() {
        let content = r#"---
name: short-drama
description: 生成短剧
tools: [file_write]
steps:
  - id: draft
    kind: agent_loop
  - id: review
    kind: chat
    depends_on: [draft]
  - id: deliver
    kind: chat
    depends_on: [review]
final: deliver
---
共享说明 body"#;
        let f = parse_flow_def(content);
        assert!(f.is_multi_step());
        assert_eq!(f.steps.len(), 3);
        assert_eq!(f.steps[0].id, "draft");
        assert_eq!(f.steps[0].kind, Some(StepKind::AgentLoop));
        assert_eq!(f.steps[1].id, "review");
        assert_eq!(f.steps[1].kind, Some(StepKind::Chat));
        assert_eq!(f.steps[1].depends_on, vec!["draft"]);
        assert_eq!(f.steps[2].depends_on, vec!["review"]);
        assert_eq!(f.final_step.as_deref(), Some("deliver"));
        assert_eq!(f.body, "共享说明 body");
    }

    #[test]
    fn step_when_and_output() {
        let content = r#"---
name: t
description: d
steps:
  - id: draft
    kind: agent_loop
  - id: revise
    kind: agent_loop
    when: "review.decision == 'revise'"
    depends_on: [draft]
    output: file:output/script.txt
---
b"#;
        let f = parse_flow_def(content);
        let revise = f.steps.iter().find(|s| s.id == "revise").unwrap();
        assert_eq!(revise.when.as_deref(), Some("review.decision == 'revise'"));
        assert_eq!(revise.output.as_deref(), Some("file:output/script.txt"));
        assert_eq!(revise.output_mode(), StepOutputMode::File("output/script.txt".into()));
    }

    #[test]
    fn step_output_json_and_llm_default() {
        let content = r#"---
name: t
description: d
steps:
  - id: a
    kind: chat
    output: json
  - id: b
    kind: chat
---
b"#;
        let f = parse_flow_def(content);
        assert_eq!(f.steps[0].output_mode(), StepOutputMode::Json);
        assert_eq!(f.steps[1].output_mode(), StepOutputMode::Llm);
    }

    #[test]
    fn step_on_failure() {
        let content = r#"---
name: t
description: d
steps:
  - id: video
    kind: agent_loop
    on_failure: fallback
  - id: fallback
    kind: agent_loop
---
b"#;
        let f = parse_flow_def(content);
        assert_eq!(f.steps[0].on_failure.as_deref(), Some("fallback"));
    }

    #[test]
    fn step_with_inline_map() {
        let content = r#"---
name: t
description: d
steps:
  - id: draft
    kind: flow_exec
    flow: script-writing
    with: {topic: "{{ input.topic }}"}
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.kind, Some(StepKind::FlowExec));
        assert_eq!(s.with.get("topic").and_then(|v| v.as_str()), Some("{{ input.topic }}"));
    }

    #[test]
    fn step_with_block_map() {
        let content = r#"---
name: t
description: d
steps:
  - id: draft
    kind: agent_loop
    with:
      topic: "{{ input.topic }}"
      count: "3"
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.with.get("topic").and_then(|v| v.as_str()), Some("{{ input.topic }}"));
        assert_eq!(s.with.get("count").and_then(|v| v.as_str()), Some("3"));
    }

    #[test]
    fn step_depends_on_block_list() {
        let content = r#"---
name: t
description: d
steps:
  - id: deliver
    kind: chat
    depends_on:
      - draft
      - review
---
b"#;
        let f = parse_flow_def(content);
        assert_eq!(f.steps[0].depends_on, vec!["draft", "review"]);
    }

    #[test]
    fn tool_step_args() {
        let content = r#"---
name: t
description: d
steps:
  - id: save
    kind: tool
    tool: file_write
    tool_args: {path: "out.txt", content: "hi"}
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.kind, Some(StepKind::Tool));
        assert_eq!(s.tool_name.as_deref(), Some("file_write"));
        assert_eq!(s.tool_args["path"].as_str(), Some("out.txt"));
        assert_eq!(s.tool_args["content"].as_str(), Some("hi"));
    }

    #[test]
    fn map_parallel_field_parses() {
        let content = r#"---
name: batch
description: d
steps:
  - id: fan
    kind: map
    over: "{{ items }}"
    as: item
    flow: sub
    parallel: 4
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.kind, Some(StepKind::Map));
        assert_eq!(s.parallel, Some(4));
        // Omitted => None (serial default).
        let content2 = r#"---
name: batch2
description: d
steps:
  - id: fan
    kind: map
    over: "{{ items }}"
    flow: sub
---
b"#;
        let f2 = parse_flow_def(content2);
        assert_eq!(f2.steps[0].parallel, None);
    }

    #[test]
    fn entry_and_top_output() {
        let content = r#"---
name: shot-image
description: d
entry: false
output: json
---
body"#;
        let f = parse_flow_def(content);
        assert_eq!(f.entry, Some(false));
        assert_eq!(f.output.as_deref(), Some("json"));
    }

    #[test]
    fn no_frontmatter_returns_body() {
        let f = parse_flow_def("just a body, no frontmatter");
        assert!(f.name.is_empty());
        assert_eq!(f.body, "just a body, no frontmatter");
    }

    #[test]
    fn empty_steps_array_is_single_step() {
        let content = "---\nname: t\ndescription: d\nsteps: []\n---\nbody";
        let f = parse_flow_def(content);
        assert!(!f.is_multi_step());
        assert!(f.steps.is_empty());
    }

    #[test]
    fn unknown_kind_preserved() {
        let content = r#"---
name: t
description: d
steps:
  - id: g
    kind: delegate
    prompt: "ok?"
---
b"#;
        let f = parse_flow_def(content);
        assert_eq!(f.steps[0].kind, Some(StepKind::Unknown("delegate".into())));
        assert!(!f.steps[0].kind.as_ref().unwrap().is_executable());
        assert_eq!(f.steps[0].prompt.as_deref(), Some("ok?"));
    }

    #[test]
    fn user_input_step_parsed() {
        let content = r#"---
name: t
description: d
steps:
  - id: review
    kind: user_input
    prompt: "继续？回复 ok/取消"
    cancel_keywords: [取消, cancel, 算了]
    timeout_hours: 24
    depends_on: [draft]
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.kind, Some(StepKind::UserInput));
        assert!(s.kind.as_ref().unwrap().is_executable());
        assert_eq!(s.cancel_keywords, vec!["取消", "cancel", "算了"]);
        assert_eq!(s.timeout_hours, Some(24.0));
        assert_eq!(s.depends_on, vec!["draft"]);
    }

    #[test]
    fn user_input_cancel_keywords_block_form() {
        let content = r#"---
name: t
description: d
steps:
  - id: review
    kind: user_input
    cancel_keywords:
      - 取消
      - cancel
---
b"#;
        let f = parse_flow_def(content);
        assert_eq!(f.steps[0].cancel_keywords, vec!["取消", "cancel"]);
    }

    #[test]
    fn flow_exec_step_parsed() {
        let content = r#"---
name: t
description: d
steps:
  - id: draft
    kind: flow_exec
    flow: script-writing
    with: {topic: "{{ input.user_message }}", count: "3"}
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.kind, Some(StepKind::FlowExec));
        assert!(s.kind.as_ref().unwrap().is_executable());
        assert_eq!(s.flow.as_deref(), Some("script-writing"));
        assert_eq!(s.with.get("topic").and_then(|v| v.as_str()), Some("{{ input.user_message }}"));
        assert_eq!(s.with.get("count").and_then(|v| v.as_str()), Some("3"));
    }

    #[test]
    fn map_step_parsed() {
        let content = r#"---
name: t
description: d
steps:
  - id: shots
    kind: map
    over: "{{ parse_shots }}"
    as: shot
    flow: shot-image
    with: {prompt: "{{ shot.prompt }}"}
    depends_on: [parse_shots]
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.kind, Some(StepKind::Map));
        assert!(s.kind.as_ref().unwrap().is_executable());
        assert_eq!(s.over.as_deref(), Some("{{ parse_shots }}"));
        assert_eq!(s.as_name.as_deref(), Some("shot"));
        assert_eq!(s.flow.as_deref(), Some("shot-image"));
        assert_eq!(s.with.get("prompt").and_then(|v| v.as_str()), Some("{{ shot.prompt }}"));
    }

    #[test]
    fn tool_step_is_executable() {
        // Tool is now executed by run_flow (stage 2 step-kind set complete).
        assert_eq!(StepKind::parse("tool"), StepKind::Tool);
        assert!(StepKind::Tool.is_executable());
        assert!(StepKind::FlowExec.is_executable());
        assert!(StepKind::Map.is_executable());
        // Unknown kinds remain non-executable.
        assert!(!StepKind::Unknown("delegate".into()).is_executable());
    }

    #[test]
    fn map_step_with_inline_body_parsed() {
        let content = r#"---
name: t
description: d
steps:
  - id: per_ep
    kind: map
    over: "{{ eps }}"
    as: ep
    body:
      - id: write
        kind: agent_loop
        output: file:out.md
      - id: review_episode
        kind: user_input
        depends_on: [write]
        prompt: "第{{ep.index}}集写完。继续/停止？"
        cancel_keywords:
          - 停止
          - stop
---
b"#;
        let f = parse_flow_def(content);
        let s = &f.steps[0];
        assert_eq!(s.kind, Some(StepKind::Map));
        assert_eq!(s.over.as_deref(), Some("{{ eps }}"));
        assert_eq!(s.as_name.as_deref(), Some("ep"));
        let body = s.body.as_ref().expect("body parsed");
        assert_eq!(body.len(), 2);
        assert_eq!(body[0].id, "write");
        assert_eq!(body[0].kind, Some(StepKind::AgentLoop));
        assert_eq!(body[0].output.as_deref(), Some("file:out.md"));
        assert_eq!(body[1].id, "review_episode");
        assert_eq!(body[1].kind, Some(StepKind::UserInput));
        assert_eq!(body[1].depends_on, vec!["write"]);
        assert_eq!(body[1].cancel_keywords, vec!["停止", "stop"]);
        assert_eq!(body[1].prompt.as_deref(), Some("第{{ep.index}}集写完。继续/停止？"));
    }

    #[test]
    fn map_step_with_block_field_after_body() {
        // A top-level field (`depends_on`) after the `body:` block must close
        // the body correctly and still be parsed.
        let content = r#"---
name: t
description: d
steps:
  - id: eps
    kind: chat
    output: json
  - id: per_ep
    kind: map
    over: "{{ eps }}"
    as: ep
    body:
      - id: write
        kind: chat
      - id: review
        kind: user_input
    depends_on: [eps]
final: per_ep
---
b"#;
        let f = parse_flow_def(content);
        assert_eq!(f.steps.len(), 2);
        let per_ep = f.steps.iter().find(|s| s.id == "per_ep").unwrap();
        assert_eq!(per_ep.body.as_ref().unwrap().len(), 2);
        assert_eq!(per_ep.depends_on, vec!["eps"]);
        assert_eq!(f.final_step.as_deref(), Some("per_ep"));
    }
}
