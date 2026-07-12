//! The engine→UI bridge: an [`EventSink`] that forwards agent and plan
//! events into the workbench's message channel.

use super::app::Msg;
use graph_core::EventSink;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

enum SinkKind {
    /// Chat agent turns: assistant text and tool activity.
    AgentTurn,
    /// Plan runs: step events and the solver stream.
    PlanRun,
}

pub struct ChannelSink {
    tx: UnboundedSender<Msg>,
    kind: SinkKind,
}

impl ChannelSink {
    pub fn agent(tx: UnboundedSender<Msg>) -> Self {
        Self {
            tx,
            kind: SinkKind::AgentTurn,
        }
    }

    pub fn plan_run(tx: UnboundedSender<Msg>) -> Self {
        Self {
            tx,
            kind: SinkKind::PlanRun,
        }
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }
}

/// A display path for a step event: nested plan frames prefix the step
/// path, and only unprefixed top-level paths map onto plan-pane rows.
fn display_path(call_stack: &[String], path: &str) -> (String, bool) {
    if call_stack.is_empty() {
        (path.to_string(), !path.contains('/'))
    } else {
        (format!("{}→{path}", call_stack.join("→")), false)
    }
}

impl EventSink for ChannelSink {
    fn text_delta(&self, text: &str) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(Msg::AgentDelta(text.to_string()));
        }
    }

    fn tool_started(&self, name: &str, _args: &Value) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(Msg::AgentToolStarted(name.to_string()));
        }
    }

    fn tool_finished(&self, name: &str, _elapsed: Duration, is_error: bool) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(Msg::AgentToolFinished {
                name: name.to_string(),
                is_error,
            });
        }
    }

    fn planning(&self) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(Msg::Planning);
        }
    }

    fn synthesizing(&self) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(Msg::Synthesizing);
        }
    }

    fn solver_delta(&self, text: &str) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(Msg::SolverDelta(text.to_string()));
        }
    }

    fn step_started(&self, call_stack: &[String], path: &str, tool: &str, input: &Value) {
        if matches!(self.kind, SinkKind::PlanRun) {
            let (path, top_level) = display_path(call_stack, path);
            self.send(Msg::StepStarted {
                path,
                tool: tool.to_string(),
                input: input.clone(),
                top_level,
            });
        }
    }

    fn step_finished(
        &self,
        call_stack: &[String],
        path: &str,
        _tool: &str,
        result: &Value,
        is_error: bool,
        _elapsed: Duration,
    ) {
        if matches!(self.kind, SinkKind::PlanRun) {
            let (path, top_level) = display_path(call_stack, path);
            self.send(Msg::StepFinished {
                path,
                result: result.clone(),
                is_error,
                top_level,
            });
        }
    }
}
