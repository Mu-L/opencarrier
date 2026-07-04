//! LoopState — unified state holder for the agent execution loop.
//!
//! Consolidates all mutable loop state that was previously scattered across
//! 10+ local variables in `run_agent_loop_impl`.

use std::collections::HashMap;
use types::message::TokenUsage;

// ---------------------------------------------------------------------------
// Budget / pressure enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BudgetState {
    Comfortable,
    Moderate,
    Tight,
    Critical,
}

#[allow(dead_code)]
impl BudgetState {
    pub fn as_label(&self) -> &'static str {
        match self {
            BudgetState::Comfortable => "comfortable",
            BudgetState::Moderate => "moderate",
            BudgetState::Tight => "tight",
            BudgetState::Critical => "critical",
        }
    }

    pub fn from_remaining_secs(secs: u64) -> Self {
        if secs > 300 {
            BudgetState::Comfortable
        } else if secs > 120 {
            BudgetState::Moderate
        } else if secs > 60 {
            BudgetState::Tight
        } else {
            BudgetState::Critical
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextPressure {
    Normal,
    Elevated,
    High,
    Critical,
}

#[allow(dead_code)]
impl ContextPressure {
    pub fn as_label(&self) -> &'static str {
        match self {
            ContextPressure::Normal => "normal",
            ContextPressure::Elevated => "elevated",
            ContextPressure::High => "high",
            ContextPressure::Critical => "critical",
        }
    }

    pub fn from_usage_pct(pct: f64) -> Self {
        if pct > 0.95 {
            ContextPressure::Critical
        } else if pct > 0.80 {
            ContextPressure::High
        } else if pct > 0.60 {
            ContextPressure::Elevated
        } else {
            ContextPressure::Normal
        }
    }
}

// ---------------------------------------------------------------------------
// Tool tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub input_hash: u64,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ToolErrorTracker {
    window_size: usize,
    history: HashMap<String, Vec<bool>>,
}

#[allow(dead_code)]
impl ToolErrorTracker {
    pub fn new(window_size: usize) -> Self {
        Self {
            window_size,
            history: HashMap::new(),
        }
    }

    pub fn record(&mut self, tool_name: &str, success: bool) {
        let entry = self.history.entry(tool_name.to_string()).or_default();
        entry.push(success);
        if entry.len() > self.window_size {
            entry.remove(0);
        }
    }

    pub fn consecutive_failures(&self, tool_name: &str) -> u32 {
        let history = match self.history.get(tool_name) {
            Some(h) => h,
            None => return 0,
        };
        let mut count = 0u32;
        for success in history.iter().rev() {
            if *success {
                break;
            }
            count += 1;
        }
        count
    }

    pub fn remove(&mut self, tool_name: &str) {
        self.history.remove(tool_name);
    }

    pub fn failed_tools(&self) -> impl Iterator<Item = (&String, u32)> {
        self.history.iter().filter_map(|(name, _)| {
            let cf = self.consecutive_failures(name);
            if cf > 0 { Some((name, cf)) } else { None }
        })
    }
}

// ---------------------------------------------------------------------------
// Turn log
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct TurnLogEntry {
    pub iteration: u32,
    pub modality: String,
    pub stop_reason: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tools_called: Vec<String>,
    pub tool_errors: u32,
    pub context_pressure: ContextPressure,
}

// ---------------------------------------------------------------------------
// Persistable run summary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub enum RunOutcome {
    Complete,
    BudgetExhausted,
    MaxIterations,
    ContextOverflow,
    Error(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct LastRunSummary {
    pub timestamp: String,
    pub iterations: u32,
    pub stop_reason: String,
    pub tokens_used: u64,
    pub outcome: RunOutcome,
}

// ---------------------------------------------------------------------------
// LoopState
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoopState {
    pub iteration: u32,
    pub max_iterations: u32,
    pub deadline: std::time::Instant,
    pub budget_state: BudgetState,
    pub budget_warning_sent: bool,
    pub context_tokens_used_estimate: usize,
    pub context_tokens_max: usize,
    pub context_pressure: ContextPressure,
    pub total_usage: TokenUsage,
    pub any_tools_executed: bool,
    pub recent_tool_calls: Vec<(String, u64)>,
    pub error_tracker: ToolErrorTracker,
    pub consecutive_max_tokens: u32,
    pub text_recovery_retries: u32,
    pub last_run: Option<LastRunSummary>,
    pub turn_log: Vec<TurnLogEntry>,
}

#[allow(dead_code)]
impl LoopState {
    pub fn new(
        max_iterations: u32,
        deadline: std::time::Instant,
        context_window_tokens: usize,
    ) -> Self {
        Self {
            iteration: 0,
            max_iterations,
            deadline,
            budget_state: BudgetState::Comfortable,
            budget_warning_sent: false,
            context_tokens_used_estimate: 0,
            context_tokens_max: context_window_tokens,
            context_pressure: ContextPressure::Normal,
            total_usage: TokenUsage::default(),
            any_tools_executed: false,
            recent_tool_calls: Vec::new(),
            error_tracker: ToolErrorTracker::new(5),
            consecutive_max_tokens: 0,
            text_recovery_retries: 0,
            last_run: None,
            turn_log: Vec::new(),
        }
    }

    pub fn remaining_secs(&self) -> u64 {
        self.deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
    }

    pub fn context_usage_pct(&self) -> f64 {
        if self.context_tokens_max == 0 {
            return 0.0;
        }
        (self.context_tokens_used_estimate as f64) / (self.context_tokens_max as f64)
    }

    pub fn refresh_budget(&mut self) {
        self.budget_state = BudgetState::from_remaining_secs(self.remaining_secs());
    }

    pub fn update_context_pressure(&mut self, estimated_tokens: usize) {
        self.context_tokens_used_estimate = estimated_tokens;
        self.context_pressure = ContextPressure::from_usage_pct(self.context_usage_pct());
    }

    pub fn log_turn(
        &mut self,
        modality: &str,
        stop_reason: &str,
        tokens_in: u32,
        tokens_out: u32,
        tools_called: Vec<String>,
        tool_errors: u32,
    ) {
        self.turn_log.push(TurnLogEntry {
            iteration: self.iteration,
            modality: modality.to_string(),
            stop_reason: stop_reason.to_string(),
            tokens_in,
            tokens_out,
            tools_called,
            tool_errors,
            context_pressure: self.context_pressure,
        });
    }

    pub fn build_status_message(
        &self,
        consecutive_tool_errors: &HashMap<String, u32>,
    ) -> String {
        let mut msg = format!(
            "📊 Turn {}/{} | ⏱️ ~{}s remaining | 📐 context: {} ({}%)",
            self.iteration + 1,
            self.max_iterations,
            self.remaining_secs(),
            self.context_pressure.as_label(),
            (self.context_usage_pct() * 100.0) as u32,
        );

        if !consecutive_tool_errors.is_empty() {
            let errors: Vec<String> = consecutive_tool_errors
                .iter()
                .map(|(name, count)| format!("{name}(×{count})"))
                .collect();
            msg.push_str(&format!("\n⚠️ 连续出错: {}", errors.join(", ")));
        }

        match self.context_pressure {
            ContextPressure::High | ContextPressure::Critical => {
                msg.push_str("\n⚠️ 上下文即将耗尽，优先输出最终答案，减少工具调用。");
            }
            _ => {}
        }

        match self.budget_state {
            BudgetState::Tight | BudgetState::Critical => {
                msg.push_str(&format!(
                    "\n⏱️ 剩余 {}s，如果工具调用可能超时，直接给出当前最佳答案。",
                    self.remaining_secs()
                ));
            }
            _ => {}
        }

        msg
    }

    pub fn to_last_run(&self, outcome: RunOutcome) -> LastRunSummary {
        LastRunSummary {
            timestamp: chrono::Utc::now().to_rfc3339(),
            iterations: self.iteration + 1,
            stop_reason: match &outcome {
                RunOutcome::Complete => "complete".to_string(),
                RunOutcome::BudgetExhausted => "budget_exhausted".to_string(),
                RunOutcome::MaxIterations => "max_iterations".to_string(),
                RunOutcome::ContextOverflow => "context_overflow".to_string(),
                RunOutcome::Error(e) => format!("error: {e}"),
            },
            tokens_used: self.total_usage.total(),
            outcome,
        }
    }
}
