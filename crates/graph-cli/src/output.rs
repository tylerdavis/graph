//! Terminal event sink: assistant text → stdout, tool activity → stderr.
//!
//! Tool calls made *inside* a plan tool (`plan__*` / `plan_and_execute`)
//! are indented under it, so the transcript reads as a tree.

use graph_core::EventSink;
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

pub struct TtySink {
    /// Suppress streaming text (final answer printed by the caller).
    quiet_text: bool,
    color: bool,
    /// Nesting depth: >0 while inside a plan tool's pipeline.
    depth: AtomicU32,
    /// Solver deltas go to stdout undimmed (plan run: the solver IS the
    /// answer) instead of dim stderr progress (chat/ask).
    solver_to_stdout: bool,
    /// True when the last stderr write was a delta without a newline.
    midline: std::sync::atomic::AtomicBool,
}

impl TtySink {
    pub fn new(quiet_text: bool) -> Self {
        Self {
            quiet_text,
            color: std::io::stderr().is_terminal(),
            depth: AtomicU32::new(0),
            solver_to_stdout: false,
            midline: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Sink for `plan run`: the solver's answer streams to stdout.
    pub fn for_plan_run() -> Self {
        Self {
            solver_to_stdout: true,
            ..Self::new(true)
        }
    }

    fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn indent(&self) -> String {
        "  ".repeat(self.depth.load(Ordering::Relaxed) as usize)
    }

    fn line(&self, text: &str) {
        if self.midline.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
        eprintln!("{}", self.dim(&format!("{}{text}", self.indent())));
    }
}

fn is_plan_tool(name: &str) -> bool {
    name.starts_with(graph_core::toolbox::PLAN_TOOL_PREFIX)
        || name == graph_core::toolbox::PLAN_AND_EXECUTE
}

impl EventSink for TtySink {
    fn text_delta(&self, text: &str) {
        if !self.quiet_text {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }

    fn tool_started(&self, name: &str, args: &Value) {
        let compact = serde_json::to_string(args).unwrap_or_default();
        let preview: String = compact.chars().take(120).collect();
        self.line(&format!("→ {name} {preview}"));
        if is_plan_tool(name) {
            self.depth.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn tool_finished(&self, name: &str, elapsed: Duration, is_error: bool) {
        if is_plan_tool(name) {
            let _ = self
                .depth
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| {
                    Some(d.saturating_sub(1))
                });
        }
        let marker = if is_error { "✗" } else { "✓" };
        self.line(&format!("{marker} {name} {elapsed:.1?}"));
    }

    fn replanning(&self, attempt: u32) {
        self.line(&format!("↻ replanning (attempt {attempt})"));
    }

    fn planning(&self) {
        self.line("✎ planning…");
    }

    fn synthesizing(&self) {
        self.line("✎ synthesizing answer…");
    }

    fn solver_delta(&self, text: &str) {
        if self.solver_to_stdout {
            print!("{text}");
            let _ = std::io::stdout().flush();
        } else {
            eprint!("{}", self.dim(text));
            let _ = std::io::stderr().flush();
            self.midline.store(!text.ends_with('\n'), Ordering::Relaxed);
        }
    }
}

/// Machine-readable event sink: one JSON line per event on stderr.
/// Selected by `GRAPH_EVENTS=jsonl` — CI logs become parseable traces.
pub struct JsonlSink {
    /// Suppress streaming text (final answer printed by the caller).
    quiet_text: bool,
}

impl JsonlSink {
    pub fn new(quiet_text: bool) -> Self {
        Self { quiet_text }
    }

    fn emit(&self, value: serde_json::Value) {
        eprintln!("{value}");
    }
}

impl EventSink for JsonlSink {
    fn text_delta(&self, text: &str) {
        if !self.quiet_text {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }

    fn tool_started(&self, name: &str, args: &Value) {
        self.emit(serde_json::json!({"event": "tool_started", "tool": name, "args": args}));
    }

    fn tool_finished(&self, name: &str, elapsed: Duration, is_error: bool) {
        self.emit(serde_json::json!({
            "event": "tool_finished", "tool": name,
            "ms": elapsed.as_millis() as u64, "is_error": is_error,
        }));
    }

    fn iteration(&self, n: u32) {
        self.emit(serde_json::json!({"event": "agent_round", "n": n}));
    }

    fn replanning(&self, attempt: u32) {
        self.emit(serde_json::json!({"event": "replanning", "attempt": attempt}));
    }

    fn planning(&self) {
        self.emit(serde_json::json!({"event": "planning"}));
    }

    fn synthesizing(&self) {
        self.emit(serde_json::json!({"event": "synthesizing"}));
    }
    // solver_delta intentionally not emitted: token-level noise.
}

/// The standard sink choice: JSONL when `GRAPH_EVENTS=jsonl`, else the TTY
/// sink (including `GRAPH_EVENTS=github`, which only adds failure
/// annotations — see [`gha_annotations`]). `solver_stdout` only applies to
/// the TTY sink (plan run).
pub fn make_sink(quiet_text: bool, solver_stdout: bool) -> std::sync::Arc<dyn EventSink> {
    if std::env::var("GRAPH_EVENTS").as_deref() == Ok("jsonl") {
        std::sync::Arc::new(JsonlSink::new(quiet_text))
    } else if solver_stdout {
        std::sync::Arc::new(TtySink::for_plan_run())
    } else {
        std::sync::Arc::new(TtySink::new(quiet_text))
    }
}

/// Surface a failure to the CI system, if an annotation mode is active.
/// Callers report every failure through this unconditionally; whether and
/// how it renders is this module's concern. `GRAPH_EVENTS=github` emits a
/// GitHub Actions `::error::` workflow command — those are only parsed from
/// stdout, the one sanctioned exception to the stdout-is-deliverable
/// contract, and only ever on failure paths, where a nonzero exit code
/// already tells automation that stdout is not a clean deliverable.
pub fn annotate_failure(message: &str) {
    if std::env::var("GRAPH_EVENTS").as_deref() == Ok("github") {
        println!("::error::{}", escape_gha_data(message));
    }
}

/// Workflow-command data encoding: `%`, `\r`, `\n` must be escaped or the
/// runner truncates the annotation at the first newline.
fn escape_gha_data(message: &str) -> String {
    message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

#[cfg(test)]
mod gha_tests {
    #[test]
    fn escapes_workflow_command_data() {
        assert_eq!(
            super::escape_gha_data("50% done\r\nnext"),
            "50%25 done%0D%0Anext"
        );
    }
}
