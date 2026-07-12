//! The UI side of debug runs: shared debugger state (breakpoints,
//! continue mode) and the execution gate that parks tool calls on a
//! oneshot until the user decides.

use super::app::{GateKind, Msg};
use async_trait::async_trait;
use graph_core::pipeline::{
    ErrorDecision, ExecutionGate, GateContext, GateDecision, PipelineError, PipelineOutcome,
    StepPath,
};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// The pause's reply, decided by the user. One enum serves both pause
/// kinds; [`UiGate`] maps it to the engine's `GateDecision` (before-call)
/// or `ErrorDecision` (on-error), applying `continue_mode` to its own
/// controls BEFORE replying — so there is no race between enabling
/// continue mode and the engine reaching the next gate.
#[derive(Debug)]
pub enum UiDecision {
    /// Before-call: proceed. On-error: let the step fail.
    Proceed {
        continue_mode: bool,
    },
    /// Before-call: skip the call — `result` becomes the step's value.
    /// On-error: replace the error with `result` and continue.
    Skip {
        result: Value,
    },
    Abort,
}

#[derive(Default)]
struct DebugState {
    breakpoints: HashSet<String>,
    continue_mode: bool,
}

/// Shared debugger state: owned by the workbench context, read by the gate
/// on the run task, written by breakpoint toggles (`b` via
/// `Effect::SyncDebug`) and the agent's run_plan breakpoints. One mutex,
/// never held across an await.
#[derive(Default)]
pub struct DebugControls(std::sync::Mutex<DebugState>);

impl DebugControls {
    pub fn set_breakpoints(&self, breakpoints: HashSet<String>) {
        self.0.lock().unwrap().breakpoints = breakpoints;
    }

    fn set_continue(&self, on: bool) {
        self.0.lock().unwrap().continue_mode = on;
    }

    /// Run start: debugger convention — run to the first breakpoint when
    /// any exist, pause at the first call when none.
    pub fn arm(&self) {
        let mut state = self.0.lock().unwrap();
        state.continue_mode = !state.breakpoints.is_empty();
    }

    /// True → the gate proceeds without asking: continue mode is on and no
    /// breakpoint matches. Breakpoints name top-level steps of the draft
    /// being debugged: a breakpoint on `E3` also pauses `E3/do.2/E10` body
    /// calls (loop-line semantics); nested-plan internals (non-empty call
    /// stack) never match — their step ids are a different namespace.
    fn auto_proceed(&self, path: &StepPath, call_stack: &[String]) -> bool {
        let state = self.0.lock().unwrap();
        state.continue_mode && !(call_stack.is_empty() && state.breakpoints.contains(&path.step))
    }

    #[cfg(test)]
    fn continuing(&self) -> bool {
        self.0.lock().unwrap().continue_mode
    }
}

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
    tx: UnboundedSender<Msg>,
    controls: std::sync::Arc<DebugControls>,
    /// One pause at a time: concurrent map items serialize their prompts
    /// instead of clobbering each other's GatePrompt in the UI.
    ask_lock: tokio::sync::Mutex<()>,
}

impl UiGate {
    pub fn new(tx: UnboundedSender<Msg>, controls: std::sync::Arc<DebugControls>) -> Self {
        Self {
            tx,
            controls,
            ask_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Park on the UI until the user decides. Applies continue-mode to the
    /// controls before returning.
    async fn ask(&self, kind: GateKind, ctx: &GateContext<'_>) -> UiDecision {
        let _guard = self.ask_lock.lock().await;
        // Any stop drops continue mode; only an explicit 'c' re-enables it.
        self.controls.set_continue(false);
        let (reply, receiver) = oneshot::channel();
        let path = if ctx.call_stack.is_empty() {
            ctx.path.to_string()
        } else {
            format!("{}→{}", ctx.call_stack.join("→"), ctx.path)
        };
        let sent = self.tx.send(Msg::GateAsk {
            kind,
            path,
            tool: ctx.tool_name.to_string(),
            input: ctx.rendered_input.clone(),
            call_stack: ctx.call_stack.to_vec(),
            scope: ctx.scope.clone(),
            reply,
        });
        if sent.is_err() {
            // UI is gone — end the run rather than free-running.
            return UiDecision::Abort;
        }
        let decision = receiver.await.unwrap_or(UiDecision::Abort);
        if let UiDecision::Proceed { continue_mode } = &decision {
            self.controls.set_continue(*continue_mode);
        }
        decision
    }
}

#[async_trait]
impl ExecutionGate for UiGate {
    async fn before_tool(&self, ctx: GateContext<'_>) -> GateDecision {
        if self.controls.auto_proceed(ctx.path, ctx.call_stack) {
            return GateDecision::Proceed;
        }
        match self.ask(GateKind::BeforeCall, &ctx).await {
            UiDecision::Proceed { .. } => GateDecision::Proceed,
            UiDecision::Skip { result } => GateDecision::Skip { result },
            UiDecision::Abort => GateDecision::Abort,
        }
    }

    /// Break-on-exception: always asks — a failing call pauses even in
    /// continue mode.
    async fn on_tool_error(&self, ctx: GateContext<'_>, error: &Value) -> ErrorDecision {
        let kind = GateKind::OnError {
            error: error.clone(),
        };
        match self.ask(kind, &ctx).await {
            UiDecision::Proceed { .. } => ErrorDecision::Fail,
            UiDecision::Skip { result } => ErrorDecision::Replace { result },
            UiDecision::Abort => ErrorDecision::Abort,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn context<'a>(
        path: &'a StepPath,
        call_stack: &'a [String],
        input: &'a Value,
        scope: &'a Map<String, Value>,
    ) -> GateContext<'a> {
        GateContext {
            path,
            tool_name: "t__x",
            rendered_input: input,
            call_stack,
            scope,
        }
    }

    /// Answer the next GateAsk on `rx` with `decision`.
    fn answer(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>, decision: UiDecision) {
        match rx.try_recv().expect("expected a GateAsk") {
            Msg::GateAsk { reply, .. } => reply.send(decision).unwrap(),
            _ => panic!("expected GateAsk"),
        }
    }

    #[tokio::test]
    async fn step_mode_asks_every_call_and_continue_skips_to_breakpoints() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controls = Arc::new(DebugControls::default());
        controls.set_breakpoints(["E3".to_string()].into());
        let gate = UiGate::new(tx, controls.clone());
        let input = json!({});
        let scope = Map::new();

        // Step mode: asks even without a breakpoint.
        let path = StepPath::top("E0");
        let ask = gate.before_tool(context(&path, &[], &input, &scope));
        tokio::pin!(ask);
        assert!(futures::poll!(ask.as_mut()).is_pending());
        answer(
            &mut rx,
            UiDecision::Proceed {
                continue_mode: true,
            },
        );
        assert!(matches!(ask.await, GateDecision::Proceed));
        assert!(controls.continuing(), "reply enabled continue mode");

        // Continue mode: non-breakpoint calls proceed without asking.
        let path = StepPath::top("E1");
        let decision = gate.before_tool(context(&path, &[], &input, &scope)).await;
        assert!(matches!(decision, GateDecision::Proceed));
        assert!(rx.try_recv().is_err(), "no prompt sent");

        // Body calls of a breakpointed step pause (loop-line semantics)…
        let path = StepPath::in_body("E3", "do.1", Some("E10"));
        let ask = gate.before_tool(context(&path, &[], &input, &scope));
        tokio::pin!(ask);
        assert!(futures::poll!(ask.as_mut()).is_pending());
        answer(
            &mut rx,
            UiDecision::Proceed {
                continue_mode: true,
            },
        );
        ask.await;

        // …but nested-plan internals never match breakpoints.
        let path = StepPath::top("E3");
        let stack = ["inner".to_string()];
        let decision = gate
            .before_tool(context(&path, &stack, &input, &scope))
            .await;
        assert!(matches!(decision, GateDecision::Proceed));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn arm_enables_continue_only_with_breakpoints() {
        let controls = DebugControls::default();
        controls.arm();
        assert!(!controls.continuing());
        controls.set_breakpoints(["E1".to_string()].into());
        controls.arm();
        assert!(controls.continuing());
    }

    #[tokio::test]
    async fn on_tool_error_asks_even_in_continue_mode() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controls = Arc::new(DebugControls::default());
        let gate = UiGate::new(tx, controls.clone());
        controls.set_continue(true);
        let input = json!({});
        let scope = Map::new();
        let error = json!({"error": "boom"});

        let path = StepPath::top("E0");
        let ask = gate.on_tool_error(context(&path, &[], &input, &scope), &error);
        tokio::pin!(ask);
        assert!(futures::poll!(ask.as_mut()).is_pending(), "error pauses");
        answer(
            &mut rx,
            UiDecision::Skip {
                result: json!("patched"),
            },
        );
        assert!(matches!(
            ask.await,
            ErrorDecision::Replace { result } if result == json!("patched")
        ));
        assert!(!controls.continuing(), "pausing dropped continue mode");
    }

    #[tokio::test]
    async fn dropped_ui_aborts_both_hooks() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let gate = UiGate::new(tx, Arc::new(DebugControls::default()));
        let input = json!({});
        let scope = Map::new();
        let path = StepPath::top("E0");
        assert!(matches!(
            gate.before_tool(context(&path, &[], &input, &scope)).await,
            GateDecision::Abort
        ));
        assert!(matches!(
            gate.on_tool_error(context(&path, &[], &input, &scope), &json!({}))
                .await,
            ErrorDecision::Abort
        ));
    }
}
