//! Tool loop detection for the agent execution loop.
//!
//! Tracks tool calls using SHA-256 hashes of `(tool_name, serialized_params)`.
//! Detects when the agent is stuck calling the same tool repeatedly and
//! provides graduated responses: warn, block, or circuit-break the entire loop.
//!
//! Features:
//! - **Hash-based repetition counting**: same tool + params → escalate
//! - **Ping-pong detection**: A-B-A-B or A-B-C-A-B-C alternating patterns
//! - **Poll tool handling**: relaxed thresholds for polling commands
//! - **Warning bucket**: upgrades to Block after repeated warnings

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Tools that are expected to be polled repeatedly.
const POLL_TOOLS: &[&str] = &["shell_exec"];

/// Maximum recent call history size for ping-pong detection.
const HISTORY_SIZE: usize = 30;

/// Configuration for the loop guard.
#[derive(Debug, Clone)]
pub struct LoopGuardConfig {
    /// Number of identical calls before a warning is appended.
    pub warn_threshold: u32,
    /// Number of identical calls before the call is blocked.
    pub block_threshold: u32,
    /// Total tool calls across all tools before circuit-breaking.
    pub global_circuit_breaker: u32,
    /// Multiplier for poll tool thresholds (poll tools get thresholds * this).
    pub poll_multiplier: u32,
    /// Minimum repeats of a ping-pong pattern before blocking.
    pub ping_pong_min_repeats: u32,
    /// Max warnings per unique tool call hash before upgrading to Block.
    pub max_warnings_per_call: u32,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            warn_threshold: 3,
            block_threshold: 5,
            global_circuit_breaker: 30,
            poll_multiplier: 3,
            ping_pong_min_repeats: 3,
            max_warnings_per_call: 3,
        }
    }
}

/// Verdict from the loop guard on whether a tool call should proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopGuardVerdict {
    /// Proceed normally.
    Allow,
    /// Proceed, but append a warning to the tool result.
    Warn(String),
    /// Block this specific tool call (skip execution).
    Block(String),
    /// Circuit-break the entire agent loop.
    CircuitBreak(String),
}

/// Tracks tool calls within a single agent loop to detect loops.
pub struct LoopGuard {
    config: LoopGuardConfig,
    /// Count of identical (tool_name + params) calls, keyed by SHA-256 hex hash.
    call_counts: HashMap<String, u32>,
    /// Total tool calls in this loop execution.
    total_calls: u32,
    /// Recent tool call hashes (ring buffer of last HISTORY_SIZE).
    recent_calls: Vec<String>,
    /// Warnings already emitted. Key = call hash, value = count emitted.
    warnings_emitted: HashMap<String, u32>,
    /// Total calls that were blocked.
    blocked_calls: u32,
    /// Map from call hash to tool name (for error messages).
    hash_to_tool: HashMap<String, String>,
}

impl LoopGuard {
    /// Create a new loop guard with the given configuration.
    pub fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            call_counts: HashMap::new(),
            total_calls: 0,
            recent_calls: Vec::with_capacity(HISTORY_SIZE),
            warnings_emitted: HashMap::new(),
            blocked_calls: 0,
            hash_to_tool: HashMap::new(),
        }
    }

    /// Check whether a tool call should proceed.
    pub fn check(&mut self, tool_name: &str, params: &serde_json::Value) -> LoopGuardVerdict {
        self.total_calls += 1;

        // Global circuit breaker
        if self.total_calls > self.config.global_circuit_breaker {
            self.blocked_calls += 1;
            return LoopGuardVerdict::CircuitBreak(format!(
                "Circuit breaker: exceeded {} total tool calls in this loop. \
                 The agent appears to be stuck.",
                self.config.global_circuit_breaker
            ));
        }

        let hash = Self::compute_hash(tool_name, params);
        self.hash_to_tool
            .entry(hash.clone())
            .or_insert_with(|| tool_name.to_string());

        // Track recent calls for ping-pong detection
        if self.recent_calls.len() >= HISTORY_SIZE {
            self.recent_calls.remove(0);
        }
        self.recent_calls.push(hash.clone());

        let count = self.call_counts.entry(hash.clone()).or_insert(0);
        *count += 1;
        let count_val = *count;

        // Determine effective thresholds (poll tools get relaxed thresholds)
        let is_poll = Self::is_poll_call(tool_name, params);
        let multiplier = if is_poll {
            self.config.poll_multiplier
        } else {
            1
        };
        let effective_warn = self.config.warn_threshold * multiplier;
        let effective_block = self.config.block_threshold * multiplier;

        // Check per-hash thresholds
        if count_val >= effective_block {
            self.blocked_calls += 1;
            return LoopGuardVerdict::Block(format!(
                "Blocked: tool '{}' called {} times with identical parameters. \
                 Try a different approach or different parameters.",
                tool_name, count_val
            ));
        }

        if count_val >= effective_warn {
            let warning_count = self.warnings_emitted.entry(hash.clone()).or_insert(0);
            *warning_count += 1;
            if *warning_count > self.config.max_warnings_per_call {
                self.blocked_calls += 1;
                return LoopGuardVerdict::Block(format!(
                    "Blocked: tool '{}' called {} times with identical parameters \
                     (warnings exhausted). Try a different approach.",
                    tool_name, count_val
                ));
            }
            return LoopGuardVerdict::Warn(format!(
                "Warning: tool '{}' has been called {} times with identical parameters. \
                 Consider a different approach.",
                tool_name, count_val
            ));
        }

        // Ping-pong detection
        if let Some(ping_pong_msg) = self.detect_ping_pong() {
            let repeats = self.count_ping_pong_repeats();
            if repeats >= self.config.ping_pong_min_repeats {
                self.blocked_calls += 1;
                return LoopGuardVerdict::Block(ping_pong_msg);
            }
            let warning_count = self
                .warnings_emitted
                .entry(format!("pingpong_{}", hash))
                .or_insert(0);
            *warning_count += 1;
            if *warning_count <= self.config.max_warnings_per_call {
                return LoopGuardVerdict::Warn(ping_pong_msg);
            }
        }

        LoopGuardVerdict::Allow
    }

    /// Check if a tool call looks like a polling operation.
    fn is_poll_call(tool_name: &str, params: &serde_json::Value) -> bool {
        if POLL_TOOLS.contains(&tool_name) {
            if let Some(cmd) = params.get("command").and_then(|v| v.as_str()) {
                let cmd_lower = cmd.to_lowercase();
                if cmd_lower.contains("status")
                    || cmd_lower.contains("poll")
                    || cmd_lower.contains("wait")
                    || cmd_lower.contains("watch")
                    || cmd_lower.contains("tail")
                    || cmd_lower.contains("ps ")
                    || cmd_lower.contains("jobs")
                    || cmd_lower.contains("pgrep")
                    || cmd_lower.contains("docker ps")
                    || cmd_lower.contains("kubectl get")
                {
                    return true;
                }
            }
        }
        let params_str = serde_json::to_string(params)
            .unwrap_or_default()
            .to_lowercase();
        params_str.contains("status") || params_str.contains("poll") || params_str.contains("wait")
    }

    /// Detect ping-pong patterns (A-B-A-B or A-B-C-A-B-C) in recent call history.
    fn detect_ping_pong(&self) -> Option<String> {
        let len = self.recent_calls.len();

        // Pattern of length 2 (A-B-A-B-A-B), need 6 entries for 3 repeats
        if len >= 6 {
            let tail = &self.recent_calls[len - 6..];
            let a = &tail[0];
            let b = &tail[1];
            if a != b && tail[2] == *a && tail[3] == *b && tail[4] == *a && tail[5] == *b {
                let tool_a = self.hash_to_tool.get(a).cloned().unwrap_or_else(|| "unknown".to_string());
                let tool_b = self.hash_to_tool.get(b).cloned().unwrap_or_else(|| "unknown".to_string());
                return Some(format!(
                    "Ping-pong detected: tools '{}' and '{}' are alternating \
                     repeatedly. Break the cycle by trying a different approach.",
                    tool_a, tool_b
                ));
            }
        }

        // Pattern of length 3 (A-B-C-A-B-C-A-B-C), need 9 entries for 3 repeats
        if len >= 9 {
            let tail = &self.recent_calls[len - 9..];
            let a = &tail[0];
            let b = &tail[1];
            let c = &tail[2];
            if !(a == b && b == c)
                && tail[3] == *a
                && tail[4] == *b
                && tail[5] == *c
                && tail[6] == *a
                && tail[7] == *b
                && tail[8] == *c
            {
                let tool_a = self.hash_to_tool.get(a).cloned().unwrap_or_else(|| "unknown".to_string());
                let tool_b = self.hash_to_tool.get(b).cloned().unwrap_or_else(|| "unknown".to_string());
                let tool_c = self.hash_to_tool.get(c).cloned().unwrap_or_else(|| "unknown".to_string());
                return Some(format!(
                    "Ping-pong detected: tools '{}', '{}', '{}' are cycling \
                     repeatedly. Break the cycle by trying a different approach.",
                    tool_a, tool_b, tool_c
                ));
            }
        }

        None
    }

    /// Count how many full repeats of the detected ping-pong pattern exist.
    fn count_ping_pong_repeats(&self) -> u32 {
        let len = self.recent_calls.len();

        // Pattern of length 2
        if len >= 4 {
            let a = &self.recent_calls[len - 2];
            let b = &self.recent_calls[len - 1];
            if a != b {
                let mut repeats: u32 = 0;
                let mut i = len;
                while i >= 2 {
                    i -= 2;
                    if self.recent_calls[i] == *a && self.recent_calls[i + 1] == *b {
                        repeats += 1;
                    } else {
                        break;
                    }
                }
                if repeats >= 2 {
                    return repeats;
                }
            }
        }

        // Pattern of length 3
        if len >= 6 {
            let a = &self.recent_calls[len - 3];
            let b = &self.recent_calls[len - 2];
            let c = &self.recent_calls[len - 1];
            if !(a == b && b == c) {
                let mut repeats: u32 = 0;
                let mut i = len;
                while i >= 3 {
                    i -= 3;
                    if self.recent_calls[i] == *a
                        && self.recent_calls[i + 1] == *b
                        && self.recent_calls[i + 2] == *c
                    {
                        repeats += 1;
                    } else {
                        break;
                    }
                }
                if repeats >= 2 {
                    return repeats;
                }
            }
        }

        0
    }

    /// Compute a SHA-256 hash of the tool name and parameters.
    fn compute_hash(tool_name: &str, params: &serde_json::Value) -> String {
        let mut hasher = Sha256::new();
        hasher.update(tool_name.as_bytes());
        hasher.update(b"|");
        let params_str = serde_json::to_string(params).unwrap_or_default();
        hasher.update(params_str.as_bytes());
        hex::encode(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_below_threshold() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({"query": "test"});
        assert_eq!(guard.check("web_search", &params), LoopGuardVerdict::Allow);
        assert_eq!(guard.check("web_search", &params), LoopGuardVerdict::Allow);
    }

    #[test]
    fn warn_at_threshold() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({"path": "/etc/passwd"});
        guard.check("file_read", &params);
        guard.check("file_read", &params);
        let v = guard.check("file_read", &params);
        assert!(matches!(v, LoopGuardVerdict::Warn(_)));
    }

    #[test]
    fn block_at_threshold() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({"command": "ls"});
        for _ in 0..4 {
            guard.check("shell_exec", &params);
        }
        let v = guard.check("shell_exec", &params);
        assert!(matches!(v, LoopGuardVerdict::Block(_)));
    }

    #[test]
    fn different_params_no_collision() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        for i in 0..10 {
            let params = serde_json::json!({"query": format!("query_{}", i)});
            assert_eq!(guard.check("web_search", &params), LoopGuardVerdict::Allow);
        }
    }

    #[test]
    fn global_circuit_breaker() {
        let config = LoopGuardConfig {
            warn_threshold: 100,
            block_threshold: 100,
            global_circuit_breaker: 5,
            ..Default::default()
        };
        let mut guard = LoopGuard::new(config);
        for i in 0..5 {
            assert_eq!(
                guard.check("tool", &serde_json::json!({"n": i})),
                LoopGuardVerdict::Allow
            );
        }
        let v = guard.check("tool", &serde_json::json!({"n": 5}));
        assert!(matches!(v, LoopGuardVerdict::CircuitBreak(_)));
    }

    #[test]
    fn default_config() {
        let config = LoopGuardConfig::default();
        assert_eq!(config.warn_threshold, 3);
        assert_eq!(config.block_threshold, 5);
        assert_eq!(config.global_circuit_breaker, 30);
    }

    #[test]
    fn ping_pong_ab_detection() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 100,
            block_threshold: 100,
            ping_pong_min_repeats: 3,
            ..Default::default()
        });
        let params_a = serde_json::json!({"file": "a.txt"});
        let params_b = serde_json::json!({"file": "b.txt"});

        guard.check("file_read", &params_a);
        guard.check("file_write", &params_b);
        guard.check("file_read", &params_a);
        guard.check("file_write", &params_b);
        guard.check("file_read", &params_a);
        let v = guard.check("file_write", &params_b);

        assert!(
            matches!(v, LoopGuardVerdict::Block(ref m) if m.contains("Ping-pong"))
                || matches!(v, LoopGuardVerdict::Warn(ref m) if m.contains("Ping-pong")),
            "Expected ping-pong detection, got: {:?}", v
        );
    }

    #[test]
    fn ping_pong_abc_detection() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 100,
            block_threshold: 100,
            ping_pong_min_repeats: 3,
            ..Default::default()
        });
        let params_a = serde_json::json!({"a": 1});
        let params_b = serde_json::json!({"b": 2});
        let params_c = serde_json::json!({"c": 3});

        for _ in 0..3 {
            guard.check("tool_a", &params_a);
            guard.check("tool_b", &params_b);
            guard.check("tool_c", &params_c);
        }
        assert!(guard.detect_ping_pong().is_some());
    }

    #[test]
    fn no_false_ping_pong() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            global_circuit_breaker: 200,
            ..Default::default()
        });
        for i in 0..10 {
            guard.check("tool", &serde_json::json!({"n": i}));
        }
        assert!(guard.detect_ping_pong().is_none());
    }

    #[test]
    fn poll_tool_relaxed_thresholds() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({"command": "docker ps --status running"});

        for _ in 0..8 {
            assert_eq!(
                guard.check("shell_exec", &params),
                LoopGuardVerdict::Allow,
                "Poll tool should have relaxed thresholds"
            );
        }
        let v = guard.check("shell_exec", &params);
        assert!(matches!(v, LoopGuardVerdict::Warn(_)));
    }

    #[test]
    fn is_poll_call_detection() {
        assert!(LoopGuard::is_poll_call("shell_exec", &serde_json::json!({"command": "docker ps --status"})));
        assert!(LoopGuard::is_poll_call("shell_exec", &serde_json::json!({"command": "tail -f /var/log/app.log"})));
        assert!(!LoopGuard::is_poll_call("shell_exec", &serde_json::json!({"command": "echo hi"})));
        assert!(!LoopGuard::is_poll_call("file_read", &serde_json::json!({"path": "/etc/hosts"})));
    }

    #[test]
    fn warning_bucket_limits() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 2,
            block_threshold: 100,
            max_warnings_per_call: 2,
            ..Default::default()
        });
        let params = serde_json::json!({"x": 1});

        assert_eq!(guard.check("tool", &params), LoopGuardVerdict::Allow);
        assert!(matches!(guard.check("tool", &params), LoopGuardVerdict::Warn(_)));
        assert!(matches!(guard.check("tool", &params), LoopGuardVerdict::Warn(_)));
        assert!(matches!(guard.check("tool", &params), LoopGuardVerdict::Block(_)));
    }

    #[test]
    fn history_ring_buffer_limit() {
        let config = LoopGuardConfig {
            warn_threshold: 100,
            block_threshold: 100,
            global_circuit_breaker: 200,
            ..Default::default()
        };
        let mut guard = LoopGuard::new(config);

        for i in 0..50 {
            guard.check("tool", &serde_json::json!({"n": i}));
        }
        assert_eq!(guard.recent_calls.len(), HISTORY_SIZE);
        assert_eq!(guard.total_calls, 50);
    }
}
