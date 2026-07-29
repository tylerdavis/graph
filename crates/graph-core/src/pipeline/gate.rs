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
    /// "do.2". Nesting appends: an `agent` step's round inside a map body
    /// is "do.2/agent.3".
    pub body: Option<String>,
    /// Innermost leaf: the body step id when the body is a step list
    /// ("E10"), or the tool name for a call an `agent` step chose.
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

    /// A call one level deeper than `self`: whatever body location `self`
    /// already carries is kept and `segment` appended, so an `agent` step
    /// running inside a map body reports "E1/do.2/agent.3/t__search"
    /// instead of collapsing onto "E1/agent.3/t__search" (which every item
    /// of a concurrent map would share).
    pub fn nested(&self, segment: &str, leaf: Option<&str>) -> Self {
        let mut body = String::new();
        for part in [self.body.as_deref(), self.body_step.as_deref()]
            .into_iter()
            .flatten()
        {
            body.push_str(part);
            body.push('/');
        }
        body.push_str(segment);
        Self {
            step: self.step.clone(),
            body: Some(body),
            body_step: leaf.map(str::to_string),
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

#[cfg(test)]
mod path_tests {
    use super::StepPath;

    #[test]
    fn nesting_keeps_the_body_location_it_started_from() {
        assert_eq!(
            StepPath::top("E1")
                .nested("agent.3", Some("t__search"))
                .to_string(),
            "E1/agent.3/t__search"
        );
        // A map body item: the item disambiguator must survive, or every
        // concurrent item reports the same path.
        assert_eq!(
            StepPath::in_body("E1", "do.2", None)
                .nested("agent.1", Some("t__search"))
                .to_string(),
            "E1/do.2/agent.1/t__search"
        );
        // A step-list branch: the body step id survives too.
        assert_eq!(
            StepPath::in_body("E2", "then", Some("B0"))
                .nested("agent.1", Some("t__search"))
                .to_string(),
            "E2/then/B0/agent.1/t__search"
        );
    }
}
