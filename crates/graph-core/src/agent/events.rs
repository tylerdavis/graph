//! Progress events emitted by the agent loop. Sinks render them for a TTY,
//! as JSONL, or (later) into a TUI.

use crate::usage::{ModelCallEvent, UsageReport};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub trait EventSink: Send + Sync {
    /// A fragment of assistant text as it streams.
    fn text_delta(&self, _text: &str) {}
    /// One model call finished, with what it cost. `site` is the step path
    /// in bus syntax (plan-qualified when nested) or a role name for calls
    /// that belong to no step — the same grouping key `by_step` uses.
    ///
    /// Emitted once per *billable* call, which is not the same as once per
    /// step: an agent step emits one per round plus one per schema repair,
    /// and a failed-over call reports the model that actually answered.
    fn model_call(&self, _call: &ModelCallEvent) {}
    /// The run's totals, once, after the last step. Carries the same report
    /// `plan run --json` embeds.
    fn usage_summary(&self, _report: &UsageReport) {}
    fn run_finished(&self, _output: &Value, _is_error: bool) {}
    fn handoff(&self, _from: &str, _to: &str, _back: bool) {}
    fn run_started(&self, _run: &RunStart) {}
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
    /// tool calls; control steps (route/map/reduce) report their raw input,
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

#[derive(Debug, Clone, Default)]
pub struct RunStart {
    pub name: String,
    pub session_id: Option<String>,
    pub input: Option<Value>,
}

/// Discards everything (used by `--json` and tests).
pub struct NullSink;

impl EventSink for NullSink {}

pub struct TeeSink {
    sinks: Vec<Arc<dyn EventSink>>,
}

impl TeeSink {
    pub fn new(sinks: Vec<Arc<dyn EventSink>>) -> Self {
        Self { sinks }
    }
}

impl EventSink for TeeSink {
    fn text_delta(&self, text: &str) {
        self.sinks.iter().for_each(|s| s.text_delta(text));
    }

    fn model_call(&self, call: &ModelCallEvent) {
        self.sinks.iter().for_each(|s| s.model_call(call));
    }

    fn usage_summary(&self, report: &UsageReport) {
        self.sinks.iter().for_each(|s| s.usage_summary(report));
    }

    fn run_finished(&self, output: &Value, is_error: bool) {
        self.sinks
            .iter()
            .for_each(|s| s.run_finished(output, is_error));
    }

    fn handoff(&self, from: &str, to: &str, back: bool) {
        self.sinks.iter().for_each(|s| s.handoff(from, to, back));
    }

    fn run_started(&self, run: &RunStart) {
        self.sinks.iter().for_each(|s| s.run_started(run));
    }

    fn tool_started(&self, name: &str, args: &Value) {
        self.sinks.iter().for_each(|s| s.tool_started(name, args));
    }

    fn tool_finished(&self, name: &str, elapsed: Duration, is_error: bool) {
        self.sinks
            .iter()
            .for_each(|s| s.tool_finished(name, elapsed, is_error));
    }

    fn iteration(&self, n: u32) {
        self.sinks.iter().for_each(|s| s.iteration(n));
    }

    fn replanning(&self, attempt: u32) {
        self.sinks.iter().for_each(|s| s.replanning(attempt));
    }

    fn planning(&self) {
        self.sinks.iter().for_each(|s| s.planning());
    }

    fn synthesizing(&self) {
        self.sinks.iter().for_each(|s| s.synthesizing());
    }

    fn solver_delta(&self, text: &str) {
        self.sinks.iter().for_each(|s| s.solver_delta(text));
    }

    fn step_started(&self, call_stack: &[String], path: &str, tool: &str, input: &Value) {
        self.sinks
            .iter()
            .for_each(|s| s.step_started(call_stack, path, tool, input));
    }

    fn step_finished(
        &self,
        call_stack: &[String],
        path: &str,
        tool: &str,
        result: &Value,
        is_error: bool,
        elapsed: Duration,
    ) {
        self.sinks
            .iter()
            .for_each(|s| s.step_finished(call_stack, path, tool, result, is_error, elapsed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    impl EventSink for Log {
        fn tool_started(&self, name: &str, _args: &Value) {
            self.0.lock().unwrap().push(format!("start {name}"));
        }
        fn step_finished(
            &self,
            call_stack: &[String],
            path: &str,
            _tool: &str,
            _result: &Value,
            is_error: bool,
            _elapsed: Duration,
        ) {
            self.0.lock().unwrap().push(format!(
                "finish {}/{path} err={is_error}",
                call_stack.join("/")
            ));
        }
    }

    #[test]
    fn a_tee_reaches_every_sink_in_order() {
        let a = Arc::new(Log::default());
        let b = Arc::new(Log::default());
        let tee = TeeSink::new(vec![a.clone(), b.clone()]);
        tee.tool_started("user__git_log", &Value::Null);
        tee.step_finished(
            &["inner".into()],
            "E1",
            "user__git_log",
            &Value::Null,
            true,
            Duration::ZERO,
        );
        let expected = vec![
            "start user__git_log".to_string(),
            "finish inner/E1 err=true".to_string(),
        ];
        assert_eq!(*a.0.lock().unwrap(), expected);
        assert_eq!(*b.0.lock().unwrap(), expected);
    }
}
