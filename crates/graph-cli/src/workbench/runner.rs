//! The UI-backed execution gate: each tool call parks on a oneshot until
//! the user decides proceed / skip / abort.

use super::app::Msg;
use async_trait::async_trait;
use graph_core::pipeline::{
    ExecutionGate, GateContext, GateDecision, PipelineError, PipelineOutcome,
};
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// One run's terminal state, shared by the keyboard path (effects) and the
/// agent's `workbench__run_plan` tool.
pub struct RunReport {
    pub headline: String,
    pub is_error: bool,
    /// Step id → result, for backfilling the plan pane.
    pub results: Map<String, Value>,
    /// A structured summary suitable as a tool result.
    pub summary: Value,
}

impl RunReport {
    pub fn finished_msg(&self) -> Msg {
        Msg::RunFinished {
            headline: self.headline.clone(),
            is_error: self.is_error,
            results: self.results.clone(),
        }
    }
}

pub fn report(result: Result<PipelineOutcome, PipelineError>) -> RunReport {
    match result {
        Ok(outcome) => {
            let exited_error = matches!(
                &outcome.exit,
                Some(e) if e.status == graph_core::pipeline::ExitStatus::Error
            );
            let headline = if let Some(exit) = &outcome.exit {
                format!(
                    "{} {}",
                    if exited_error {
                        "✗ exited:"
                    } else {
                        "✓ exited:"
                    },
                    exit.message
                )
            } else if let Some(structured) = &outcome.structured {
                format!("✓ output: {}", truncate(&structured.to_string(), 120))
            } else if outcome.answer.is_empty() {
                format!("✓ completed ({} steps)", outcome.state.steps_executed())
            } else {
                "✓ completed — answer in the run tab".to_string()
            };
            let summary = json!({
                "status": if outcome.exit.is_none() {
                    "completed"
                } else if exited_error {
                    "exited_error"
                } else {
                    "exited_success"
                },
                "exitMessage": outcome.exit.as_ref().map(|e| e.message.clone()),
                "answer": (!outcome.answer.is_empty()).then_some(&outcome.answer),
                "output": outcome.structured,
                "stepsExecuted": outcome.state.steps_executed(),
            });
            RunReport {
                headline,
                is_error: exited_error,
                results: outcome.state.results,
                summary,
            }
        }
        Err(PipelineError::Aborted { step, state }) => RunReport {
            headline: format!("⊘ aborted at {step}"),
            is_error: true,
            summary: json!({"status": "aborted", "step": step}),
            results: state.results,
        },
        Err(error) => RunReport {
            headline: format!("✗ {error}"),
            is_error: true,
            results: Map::new(),
            summary: json!({"status": "failed", "error": error.to_string()}),
        },
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

pub struct UiGate {
    pub tx: UnboundedSender<Msg>,
}

#[async_trait]
impl ExecutionGate for UiGate {
    async fn before_tool(&self, ctx: GateContext<'_>) -> GateDecision {
        let (reply, receiver) = oneshot::channel();
        let path = if ctx.call_stack.is_empty() {
            ctx.path.to_string()
        } else {
            format!("{}→{}", ctx.call_stack.join("→"), ctx.path)
        };
        let sent = self.tx.send(Msg::GateAsk {
            path,
            tool: ctx.tool_name.to_string(),
            input: ctx.rendered_input.clone(),
            reply,
        });
        if sent.is_err() {
            // UI is gone — end the run rather than free-running.
            return GateDecision::Abort;
        }
        receiver.await.unwrap_or(GateDecision::Abort)
    }
}
