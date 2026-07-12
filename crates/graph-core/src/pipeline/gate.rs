//! Execution gating: an optional hook consulted before every real tool
//! dispatch — registry tools, `plan__*` steps, `plan_and_execute`, and
//! every call inside decide branches and map/reduce bodies, at any plan
//! nesting depth. It lets an interactive caller (the workbench) pause a
//! run for confirmation, skip a call by injecting its result, or abort.
//!
//! The gate is NOT consulted for control-step evaluation — `exit` gates,
//! `decide` gates (including `infer` judge LLM calls), and map/reduce
//! orchestration are read-only engine computation with no external effect;
//! their side effects are the body calls, which are gated.

use async_trait::async_trait;
use serde_json::{Map, Value};
use std::fmt;

/// Where a tool call sits in the plan. Displays with the bus-source
/// syntax: "E3", "E3/then", "E3/do.2", "E3/do.2/E10".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepPath {
    /// Top-level step id ("E3").
    pub step: String,
    /// Body location for calls inside decide/map/reduce: "then", "else",
    /// "do.2".
    pub body: Option<String>,
    /// Inner step id when the body is a step list ("E10").
    pub body_step: Option<String>,
}

impl StepPath {
    pub fn top(step: &str) -> Self {
        Self {
            step: step.to_string(),
            body: None,
            body_step: None,
        }
    }

    pub fn in_body(step: &str, body: &str, body_step: Option<&str>) -> Self {
        Self {
            step: step.to_string(),
            body: Some(body.to_string()),
            body_step: body_step.map(str::to_string),
        }
    }
}

impl fmt::Display for StepPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.step)?;
        if let Some(body) = &self.body {
            write!(f, "/{body}")?;
        }
        if let Some(body_step) = &self.body_step {
            write!(f, "/{body_step}")?;
        }
        Ok(())
    }
}

/// Everything a gate sees about the call it is deciding on.
pub struct GateContext<'a> {
    pub path: &'a StepPath,
    pub tool_name: &'a str,
    /// Fully rendered input — exactly what the tool would receive.
    pub rendered_input: &'a Value,
    /// Plan-call nesting; empty at the top level. Frames are plan
    /// identifiers or "plan_and_execute".
    pub call_stack: &'a [String],
    /// The template scope the input was rendered against — the debugger's
    /// "locals". At the top level: the run's results map (`input` plus
    /// prior step results). Inside decide/map/reduce bodies: the layered
    /// body scope, including the `item`/`index`/`accumulator` pseudo-roots
    /// and earlier same-body step ids.
    pub scope: &'a Map<String, Value>,
}

pub enum GateDecision {
    /// Make the call.
    Proceed,
    /// Do not call the tool; `result` becomes the step's value exactly as
    /// if the tool had returned it (downstream templates render against it).
    Skip { result: Value },
    /// End the run now — no replan, no solver, no error summary. Surfaces
    /// as [`super::PipelineError::Aborted`] carrying the partial run state.
    Abort,
}

/// How a gate resolves a failed tool call.
pub enum ErrorDecision {
    /// The step fails exactly as without a gate: `StepFailed` on explicit
    /// runs, replan-eligible on planned runs.
    Fail,
    /// Substitute `result` and continue exactly as if the tool had
    /// returned it. The replacement never enters the replan loop.
    Replace { result: Value },
    /// End the run now (same semantics as [`GateDecision::Abort`]).
    Abort,
}

/// Consulted before every real tool dispatch (see module docs for scope).
/// May be called concurrently when `map` runs with `concurrency` above 1 —
/// implementations that prompt a user should serialize internally.
#[async_trait]
pub trait ExecutionGate: Send + Sync {
    async fn before_tool(&self, ctx: GateContext<'_>) -> GateDecision;

    /// Consulted after a dispatched call returns an error, before the
    /// error propagates — the debugger's break-on-exception. The default
    /// preserves ungated behavior. Not consulted when a nested run was
    /// aborted (aborts stay hard) and never for control-step evaluation.
    async fn on_tool_error(&self, _ctx: GateContext<'_>, _error: &Value) -> ErrorDecision {
        ErrorDecision::Fail
    }
}
