//! Progress events emitted by the agent loop. Sinks render them for a TTY,
//! as JSONL, or (later) into a TUI.

use serde_json::Value;
use std::time::Duration;

pub trait EventSink: Send + Sync {
    /// A fragment of assistant text as it streams.
    fn text_delta(&self, _text: &str) {}
    /// A tool invocation is starting.
    fn tool_started(&self, _name: &str, _args: &Value) {}
    /// A tool invocation finished.
    fn tool_finished(&self, _name: &str, _elapsed: Duration, _is_error: bool) {}
    /// The model requested tools and the agent loop is going around again.
    fn iteration(&self, _n: u32) {}
    /// The pipeline discarded a defective plan and is replanning.
    fn replanning(&self, _attempt: u32) {}
    /// The pipeline's planner is authoring a plan (LLM call, can be slow).
    fn planning(&self) {}
    /// The pipeline's solver is synthesizing the answer (LLM call over the
    /// collected data — often the longest single wait in a plan run).
    fn synthesizing(&self) {}
    /// A fragment of the solver's answer as it streams.
    fn solver_delta(&self, _text: &str) {}
    /// A plan step (or body call) is starting. `path` uses the bus-source
    /// syntax — "E3", "E3/then", "E3/do.2/E10" — and `call_stack` is the
    /// plan-call nesting (empty at the top level), disambiguating an inner
    /// plan's "E0" from the outer plan's. `input` is the rendered input for
    /// tool calls; control steps (decide/map/reduce) report their raw input,
    /// since their bodies render lazily.
    fn step_started(&self, _call_stack: &[String], _path: &str, _tool: &str, _input: &Value) {}
    /// A plan step (or body call) finished, carrying its full result value —
    /// including body-scoped results that never enter the run's results map.
    fn step_finished(
        &self,
        _call_stack: &[String],
        _path: &str,
        _tool: &str,
        _result: &Value,
        _is_error: bool,
        _elapsed: Duration,
    ) {
    }
}

/// Discards everything (used by `--json` and tests).
pub struct NullSink;

impl EventSink for NullSink {}
