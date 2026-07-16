//! The engine→UI bridge: an [`EventSink`] that forwards agent and plan
//! events into the workbench's message channel.

use super::app::Msg;
use super::plan_ws::OutlineRow;
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
/// path, and only unprefixed paths — the run plan's own steps and their
/// body sub-steps — map onto plan-pane rows.
fn display_path(call_stack: &[String], path: &str) -> (String, bool) {
    if call_stack.is_empty() {
        (path.to_string(), true)
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

    fn draft_outline(&self, items: &Value) {
        if matches!(self.kind, SinkKind::PlanRun) {
            let items = items
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|item| OutlineRow {
                            summary: item
                                .get("summary")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            expected_tool: item
                                .get("expectedTool")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.send(Msg::DraftOutline { items });
        }
    }

    fn draft_step_started(&self, index: usize, summary: &str) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(Msg::DraftStepStarted {
                index,
                summary: summary.to_string(),
            });
        }
    }

    fn draft_step_finished(&self, index: usize, step: &Value, problems: &[String], attempt: u32) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(Msg::DraftStepFinished {
                index,
                step: step.clone(),
                problems: problems.to_vec(),
                attempt,
            });
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
            let (path, in_plan) = display_path(call_stack, path);
            self.send(Msg::StepStarted {
                path,
                tool: tool.to_string(),
                input: input.clone(),
                in_plan,
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
            let (path, in_plan) = display_path(call_stack, path);
            self.send(Msg::StepFinished {
                path,
                result: result.clone(),
                is_error,
                in_plan,
            });
        }
    }
}
