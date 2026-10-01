//! The effect executor: everything the pure reducer can't do. Long-running
//! work is spawned; completion always arrives back as a [`Msg`].

use super::app::{Effect, Msg};
use super::runner::{DebugControls, UiGate, UiInterlocutor};
use super::tools::DraftState;
use graph_core::agent::conversation::{Conversation, ConversationError};
use graph_core::pipeline::authoring;
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::Pipeline;
use graph_core::{AgentError, NewEntry, Store, ToolRegistry};
use serde_json::Map;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub struct WorkbenchContext {
    pub conversation: Conversation,
    pub active: std::sync::Mutex<String>,
    /// The plan-run pipeline (its sink already feeds the UI channel);
    /// gated runs clone it and install a [`UiGate`].
    pub pipeline: Arc<Pipeline>,
    pub history: Arc<tokio::sync::Mutex<Vec<NewEntry>>>,
    /// The draft plan, shared with [`super::tools::WorkbenchTools`].
    pub draft: Arc<std::sync::Mutex<DraftState>>,
    /// The agent's full catalog — the context pane shows what the planner
    /// and agent can call.
    pub catalog: Arc<dyn ToolRegistry>,
    pub store: Arc<dyn Store>,
    /// Where unsaved drafts land on Ctrl+S (first configured plans dir).
    pub plans_dir: Option<PathBuf>,
    /// Shared debugger state (breakpoints, continue mode) read by the gate.
    pub debug: Arc<DebugControls>,
    pub tx: UnboundedSender<Msg>,
}

pub fn run_effect(effect: Effect, context: &Arc<WorkbenchContext>) {
    let ctx = context.clone();
    match effect {
        Effect::RunAgentTurn { message } => {
            tokio::spawn(async move {
                tracing::debug!(
                    target: "workbench",
                    "agent turn started ({} chars)",
                    message.len()
                );
                let turn_started = std::time::Instant::now();
                let events = ctx.conversation.events.clone();
                let active = ctx.active.lock().unwrap().clone();
                let mut history = ctx.history.lock().await;
                events.run_started(&graph_core::RunStart {
                    name: "workbench".to_string(),
                    session_id: None,
                    input: Some(serde_json::Value::String(message.clone())),
                });
                let result = ctx.conversation.run_turn(&history, &active, &message).await;
                match &result {
                    Ok(turn) => {
                        events.run_finished(&serde_json::Value::String(turn.text.clone()), false)
                    }
                    Err(error) => {
                        events.run_finished(&serde_json::json!({"error": error.to_string()}), true)
                    }
                }
                let result = match result {
                    Ok(turn) => {
                        history.extend(turn.entries);
                        if turn.active != active {
                            *ctx.active.lock().unwrap() = turn.active.clone();
                            let _ = ctx.tx.send(Msg::ActiveAgent {
                                name: turn.active.clone(),
                                note: Some(format!("⇢ {active} handed off to {}", turn.active)),
                            });
                        }
                        Ok(turn.text)
                    }
                    Err(ConversationError::Turn {
                        source, partial, ..
                    }) => {
                        if keep_partial_history(&source) {
                            history.extend(partial);
                        }
                        Err(turn_failure_message(source))
                    }
                    Err(other) => Err(other.to_string()),
                };
                // Per turn, matching `graph chat`. The ledger is shared with
                // the pipeline, so a turn that called a plan tool reports that
                // plan's spend too.
                let usage = ctx.pipeline.usage.take();
                if !usage.is_empty() {
                    events.usage_summary(&usage);
                }
                tracing::debug!(
                    target: "workbench",
                    "agent turn took {:.1}s ({} entries in history)",
                    turn_started.elapsed().as_secs_f64(),
                    history.len()
                );
                let _ = ctx.tx.send(Msg::TurnFinished(result));
            });
        }

        Effect::Agent { name } => {
            let msg = match name {
                None => {
                    let active = ctx.active.lock().unwrap().clone();
                    let lines: Vec<String> = ctx
                        .conversation
                        .agents
                        .iter()
                        .map(|doc| {
                            let marker = if doc.name == active { "*" } else { " " };
                            format!("{marker} {} — {}", doc.name, doc.description)
                        })
                        .collect();
                    Msg::AgentNotice(format!("talking with {active}\n{}", lines.join("\n")))
                }
                Some(name) => match ctx.conversation.agent(&name) {
                    Ok(doc) => {
                        *ctx.active.lock().unwrap() = doc.name.clone();
                        Msg::ActiveAgent {
                            name: doc.name.clone(),
                            note: Some(format!("now talking with {}", doc.name)),
                        }
                    }
                    Err(error) => Msg::AgentNotice(error.to_string()),
                },
            };
            let _ = ctx.tx.send(msg);
        }

        Effect::StartRun { gated, input } => {
            tokio::spawn(async move {
                let doc = { ctx.draft.lock().unwrap().doc.clone() };
                let Some(doc) = doc else {
                    let _ = ctx.tx.send(Msg::RunFinished {
                        headline: "no plan to run".to_string(),
                        is_error: true,
                        exited: false,
                        results: Map::new(),
                    });
                    return;
                };
                let mut pipeline = (*ctx.pipeline)
                    .clone()
                    .with_interlocutor(Arc::new(UiInterlocutor::new(ctx.tx.clone())));
                if gated {
                    ctx.debug.arm();
                    pipeline = pipeline
                        .with_gate(Arc::new(UiGate::new(ctx.tx.clone(), ctx.debug.clone())));
                }
                tracing::debug!(
                    target: "workbench",
                    "run started: '{}' ({} steps, gated={gated})",
                    doc.identifier,
                    doc.steps.len()
                );
                let run_started = std::time::Instant::now();
                pipeline.events.run_started(&graph_core::RunStart {
                    name: doc.identifier.clone(),
                    session_id: None,
                    input: Some(input.clone()),
                });
                let query = format!("Run the '{}' plan", doc.name);
                let result = pipeline
                    .run_explicit(&query, doc.steps.clone(), doc.finish(), Some(input))
                    .await;
                crate::telemetry::report_plan_result(pipeline.events.as_ref(), &result);
                tracing::debug!(
                    target: "workbench",
                    "run took {:.1}s",
                    run_started.elapsed().as_secs_f64()
                );
                // Before the finished message, so the run log reads
                // step → usage → verdict. Drained even on a failed run: the
                // tokens were still spent.
                let usage = pipeline.usage.take();
                if !usage.is_empty() {
                    pipeline.events.usage_summary(&usage);
                }
                let msg = super::runner::report(result).finished_msg();
                let _ = ctx.tx.send(msg);
            });
        }

        Effect::Validate => {
            let problems = match &ctx.draft.lock().unwrap().doc {
                Some(doc) => super::tools::plan_problems(&ctx.pipeline, doc),
                None => vec!["no draft to validate".to_string()],
            };
            let _ = ctx.tx.send(Msg::Validated(problems));
        }

        Effect::LoadContext => {
            tokio::spawn(async move {
                // The registry alone under-reports what the planner sees:
                // control steps are executor-evaluated, never registered, so
                // their descriptions are appended here just like the `tools`
                // command surface does.
                let mut tools = ctx.catalog.tools().await.unwrap_or_default();
                tools.extend(graph_core::pipeline::control_step_defs());
                let shapes = ctx.store.tool_shapes().await.unwrap_or_default();
                let _ = ctx.tx.send(Msg::ContextLoaded { tools, shapes });
            });
        }

        Effect::SyncDebug { breakpoints } => {
            ctx.debug.set_breakpoints(breakpoints);
        }

        Effect::ApplyEdit { edit, commit } => {
            let current = { ctx.draft.lock().unwrap().doc.clone() };
            let msg = match current {
                None => Msg::EditOutcome {
                    committed: false,
                    introduced: vec!["no draft to edit".to_string()],
                    pre_existing: Vec::new(),
                },
                Some(doc) if commit => match authoring::apply_edit(&doc, |d| edit.apply(d)) {
                    Ok(accepted) => {
                        super::tools::publish_draft(&ctx.draft, &ctx.tx, accepted.doc, true);
                        Msg::EditOutcome {
                            committed: true,
                            introduced: Vec::new(),
                            pre_existing: accepted.pre_existing,
                        }
                    }
                    Err(rejected) => Msg::EditOutcome {
                        committed: false,
                        introduced: rejection_problems(&rejected),
                        pre_existing: rejected.pre_existing,
                    },
                },
                Some(doc) => check_edit(&ctx, &doc, &edit),
            };
            let _ = ctx.tx.send(msg);
        }

        Effect::SavePlan => {
            let result = save_draft(&ctx.draft, ctx.plans_dir.as_deref());
            let _ = ctx.tx.send(Msg::Saved(result));
        }

        Effect::RestoreDraft => {
            let restored = ctx.draft.lock().unwrap().restore();
            match restored {
                Some((doc, dirty)) => {
                    let _ = ctx.tx.send(Msg::DraftReplaced {
                        doc: Box::new(doc),
                        dirty,
                    });
                }
                None => {
                    let _ = ctx.tx.send(Msg::Status(
                        "nothing to restore — the draft has not been replaced yet".to_string(),
                    ));
                }
            }
        }
    }
}

fn rejection_problems(rejected: &authoring::EditRejected) -> Vec<String> {
    if !rejected.introduced.is_empty() {
        return rejected.introduced.clone();
    }
    rejected
        .body
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(|error| vec![error.to_string()])
        .unwrap_or_default()
}

fn check_edit(ctx: &WorkbenchContext, doc: &PlanDoc, edit: &super::edit::PendingEdit) -> Msg {
    let before = super::tools::plan_problems(&ctx.pipeline, doc);
    let mut edited = doc.clone();
    if let Err(body) = edit.apply(&mut edited) {
        return Msg::EditOutcome {
            committed: false,
            introduced: body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(|error| vec![error.to_string()])
                .unwrap_or_default(),
            pre_existing: before,
        };
    }
    let after = super::tools::plan_problems(&ctx.pipeline, &edited);
    let blocking = authoring::static_problems(&edited);
    let (pre_existing, introduced): (Vec<String>, Vec<String>) = after
        .into_iter()
        .partition(|problem| before.contains(problem));
    let introduced = introduced
        .into_iter()
        .map(|problem| {
            if blocking.contains(&problem) {
                problem
            } else {
                format!("non-blocking (submit still allowed): {problem}")
            }
        })
        .collect();
    Msg::EditOutcome {
        committed: false,
        introduced,
        pre_existing,
    }
}

/// MaxIterations is real progress — the history ends cleanly in tool
/// results, so the next message can build on it. Every other turn error
/// leaves the exchange broken mid-turn and must be rolled back.
fn keep_partial_history(error: &AgentError) -> bool {
    matches!(error, AgentError::MaxIterations(_))
}

fn turn_failure_message(error: AgentError) -> String {
    match error {
        AgentError::MaxIterations(n) => format!(
            "stopped after {n} tool iterations with no successful edit — \
             progress is kept, send \"continue\" to resume (or raise \
             max_agent_iterations in config)"
        ),
        other => other.to_string(),
    }
}

/// Write the draft to disk: back to its source file, or into the plans
/// directory for new drafts. Shared by Ctrl+S and `workbench__save_plan`.
pub fn save_draft(
    draft: &std::sync::Mutex<DraftState>,
    plans_dir: Option<&std::path::Path>,
) -> Result<String, String> {
    let mut guard = draft.lock().unwrap();
    let Some(doc) = guard.doc.as_mut() else {
        return Err("no draft".to_string());
    };
    // Where it lands, and the guards against clobbering another plan, are
    // shared with `graph plan` via authoring::target_path / write_doc. The
    // draft's on-disk identity and dirty flag are this surface's own
    // bookkeeping, so they stay here.
    let path = authoring::target_path(doc, plans_dir).map_err(|e| e.to_string())?;
    authoring::write_doc(doc, &path, false).map_err(|e| e.to_string())?;
    doc.path = Some(path.clone());
    guard.dirty = false;
    Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_iterations_keeps_history_and_says_how_to_continue() {
        assert!(keep_partial_history(&AgentError::MaxIterations(15)));
        assert!(!keep_partial_history(&AgentError::IncompleteStream));

        let message = turn_failure_message(AgentError::MaxIterations(15));
        assert!(message.contains("15"), "{message}");
        assert!(message.contains("continue"), "{message}");
        assert!(message.contains("max_agent_iterations"), "{message}");

        let other = turn_failure_message(AgentError::IncompleteStream);
        assert!(other.contains("stream ended"), "{other}");
    }
}
