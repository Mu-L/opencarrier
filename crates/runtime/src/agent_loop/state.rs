//! LoopState — unified state holder for the agent execution loop.
//!
//! Consolidates all mutable loop state that was previously scattered across
//! 10+ local variables in `run_agent_loop_impl`.

use std::collections::HashMap;
use types::message::TokenUsage;

// ---------------------------------------------------------------------------
// Context pressure
// ---------------------------------------------------------------------------

/// Turn-end is governed by progress/stuck detection (tool-call repetition via
/// `BreakToolLoop` + no-progress idle), NOT a wall-clock budget. The outer turn
/// timeout (`agent_turn_timeout_secs`, default 4h) is a generous daemon-hang
/// backstop only - it must never kill legitimate long work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextPressure {
    Normal,
    Elevated,
    High,
    Critical,
}

impl ContextPressure {
    pub fn as_label(&self) -> &'static str {
        match self {
            ContextPressure::Normal => "normal",
            ContextPressure::Elevated => "elevated",
            ContextPressure::High => "high",
            ContextPressure::Critical => "critical",
        }
    }
}

// ---------------------------------------------------------------------------
// Tool tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolErrorTracker {
    window_size: usize,
    history: HashMap<String, Vec<bool>>,
}

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
pub enum RunOutcome {
    Complete,
    BudgetExhausted,
    MaxIterations,
    ContextOverflow,
    Error(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
pub struct LoopState {
    pub iteration: u32,
    /// Consecutive iterations that made no progress (no tool call, no final
    /// answer, not actively generating via MaxTokens). Reaches
    /// [`super::NO_PROGRESS_THRESHOLD`] -> turn aborted as stuck.
    pub idle_streak: u32,
    /// Tools executed in the CURRENT iteration. Reset at the top of each
    /// `loop_iteration`; bumped per SUCCESSFUL tool call in `tool_use` (failed
    /// calls don't count - an all-failed iteration is treated as no-progress).
    /// Drives the no-progress detector.
    pub tools_this_iter: u32,
    pub context_tokens_used_estimate: usize,
    pub context_tokens_max: usize,
    pub context_pressure: ContextPressure,
    pub total_usage: TokenUsage,
    pub any_tools_executed: bool,
    pub recent_tool_calls: Vec<(String, u64)>,
    pub error_tracker: ToolErrorTracker,
    /// Per-tool count of how many times loop detection has fired for the exact
    /// same `(name, input_hash)` within this turn. Used to ESCALATE corrective
    /// guidance (2nd nudge is stronger) and, after [`super::tool_use::LOOP_BREAK_THRESHOLD`],
    /// fail the turn fast instead of silently burning the whole `max_iterations`
    /// budget on a stuck loop. (The tool itself is never removed — we educate,
    /// not punish; this is only the escalation counter.)
    pub tool_loop_rearm: HashMap<String, u32>,
    /// Per-`(tool_name, input_hash)` call count across the WHOLE turn (not the
    /// sliding `recent_tool_calls` window). Survives `recent_tool_calls.clear()`.
    /// Distinct from `tool_loop_rearm` (per-NAME escalation, only bumps on
    /// consecutive-window hits). When any single pair reaches
    /// [`super::helpers::CUMULATIVE_LOOP_THRESHOLD`], the turn is aborted — this
    /// catches ROTATING repetition (e.g. file_read on 4 paths cycled, each read
    /// 3× total but never 4-in-a-row) that the consecutive-only window misses.
    pub tool_call_counts: HashMap<(String, u64), u32>,
    pub consecutive_max_tokens: u32,
    pub text_recovery_retries: u32,
    pub last_run: Option<LastRunSummary>,
    pub turn_log: Vec<TurnLogEntry>,
}

impl LoopState {
    pub fn new(context_window_tokens: usize) -> Self {
        Self {
            iteration: 0,
            idle_streak: 0,
            tools_this_iter: 0,
            context_tokens_used_estimate: 0,
            context_tokens_max: context_window_tokens,
            context_pressure: ContextPressure::Normal,
            total_usage: TokenUsage::default(),
            any_tools_executed: false,
            recent_tool_calls: Vec::new(),
            error_tracker: ToolErrorTracker::new(5),
            tool_loop_rearm: HashMap::new(),
            tool_call_counts: HashMap::new(),
            consecutive_max_tokens: 0,
            text_recovery_retries: 0,
            last_run: None,
            turn_log: Vec::new(),
        }
    }

    pub fn context_usage_pct(&self) -> f64 {
        if self.context_tokens_max == 0 {
            return 0.0;
        }
        (self.context_tokens_used_estimate as f64) / (self.context_tokens_max as f64)
    }

    /// Record whether the just-completed iteration made progress, and return
    /// `Some(idle_streak)` when the no-progress threshold is reached (caller
    /// should abort the turn as stuck), else `None`.
    ///
    /// "Progress" = the iteration called at least one SUCCESSFUL tool, produced a
    /// final answer, or was actively generating (MaxTokens). A ToolUse iteration
    /// where every tool errored (or an EndTurn/StopSequence spin with no tools)
    /// counts as idle. Only consecutive idle turns trip the threshold.
    pub fn record_iteration_progress(&mut self, made_progress: bool) -> Option<u32> {
        if made_progress {
            self.idle_streak = 0;
            return None;
        }
        self.idle_streak += 1;
        if self.idle_streak >= super::NO_PROGRESS_THRESHOLD {
            Some(self.idle_streak)
        } else {
            None
        }
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

    pub fn build_status_message(&self) -> String {
        let mut msg = format!(
            "📊 Turn {} | 📐 context: {} ({}%)",
            self.iteration + 1,
            self.context_pressure.as_label(),
            (self.context_usage_pct() * 100.0) as u32,
        );

        // Soft loop detection: same tool called consecutively
        if let Some(name) = super::helpers::detect_soft_loop(
            &self.recent_tool_calls,
            super::helpers::SOFT_LOOP_WINDOW,
        ) {
            msg.push_str(&format!(
                "\n💡 工具 `{name}` 连续被调用，确认这不是重复操作？如果是分页/批量则忽略。"
            ));
        }

        // Error tracking via sliding window
        let failed: Vec<String> = self.error_tracker.failed_tools()
            .map(|(name, count)| format!("{name}(×{count})"))
            .collect();
        if !failed.is_empty() {
            msg.push_str(&format!("\n⚠️ 连续出错: {}", failed.join(", ")));
        }

        match self.context_pressure {
            ContextPressure::High | ContextPressure::Critical => {
                msg.push_str("\n⚠️ 上下文即将耗尽，优先输出最终答案，减少工具调用。");
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
