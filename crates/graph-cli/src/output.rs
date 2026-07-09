//! Terminal event sink: assistant text → stdout, tool activity → stderr.

use graph_core::EventSink;
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::time::Duration;

pub struct TtySink {
    /// Suppress streaming text (final answer printed by the caller).
    quiet_text: bool,
    color: bool,
}

impl TtySink {
    pub fn new(quiet_text: bool) -> Self {
        Self {
            quiet_text,
            color: std::io::stderr().is_terminal(),
        }
    }

    fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
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
        eprintln!("{}", self.dim(&format!("→ {name} {preview}")));
    }

    fn tool_finished(&self, name: &str, elapsed: Duration, is_error: bool) {
        let marker = if is_error { "✗" } else { "✓" };
        eprintln!("{}", self.dim(&format!("{marker} {name} {elapsed:.1?}")));
    }

    fn replanning(&self, attempt: u32) {
        eprintln!("{}", self.dim(&format!("↻ replanning (attempt {attempt})")));
    }
}
