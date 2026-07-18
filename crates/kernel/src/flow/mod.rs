//! Multi-step flow DAG executor (`run_flow`).
//!
//! Executes a [`types::flow::FlowDef`] with non-empty `steps` as a topologically
//! ordered DAG. Split into focused submodules; the public surface is re-exported
//! from [`crate::flow_runner`] for existing call sites.

mod dag;
mod map;
mod report;
mod run;
mod steps;
mod subflow;
mod template;
mod types;

#[cfg(test)]
mod tests;

pub(crate) use types::{
    FlowOutcome, MapContext, MapOutcome, ResumeState, FLOW_DEPTH, FAILURE_CANCEL_KEYWORDS,
    MAX_FLOW_DEPTH,
};
