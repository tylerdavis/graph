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
