//! The effect executor: everything the pure reducer can't do. Long-running
//! work is spawned; completion always arrives back as a [`Msg`].

use super::app::{Effect, Msg};
use super::runner::{DebugControls, UiGate};
use super::tools::DraftState;
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::Pipeline;
use graph_core::{Agent, AgentError, Store, ToolRegistry};
use graph_llm::types::ChatMessage;
use serde_json::Map;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub struct WorkbenchContext {
    pub agent: Agent,
    /// The plan-run pipeline (its sink already feeds the UI channel);
    /// gated runs clone it and install a [`UiGate`].
    pub pipeline: Arc<Pipeline>,
    /// Chat history — caller-owned per the `run_turn` contract, shared
    /// with the turn task.
    pub history: Arc<tokio::sync::Mutex<Vec<ChatMessage>>>,
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

/// The system prompt for one turn: the base prompt plus the CURRENT draft,
/// so the agent always sees what the plan pane shows — no read-back call
/// needed when the user says "debug this plan".
fn turn_system_prompt(base: &str, draft: &Option<PlanDoc>) -> String {
    let mut prompt = base.to_string();
    prompt.push_str("\n\n## Current draft\n");
    match draft {
        Some(doc) => {
            prompt.push_str(&format!(
                "The plan pane currently shows '{}' — this YAML is current as \
                 of this turn, so do NOT call workbench__get_plan just to read \
                 it (only to re-check after your own edits within this turn):\n",
                doc.identifier
            ));
            match serde_yaml::to_string(doc) {
                Ok(yaml) => prompt.push_str(&yaml),
                Err(_) => prompt.push_str("(unserializable draft — use workbench__get_plan)"),
            }
        }
        None => prompt.push_str(
            "(none yet — the pane is empty; draft one with workbench__draft_plan \
             or load one with workbench__load_plan when asked)",
        ),
    }
    prompt
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
                // Rebuild the agent with the draft baked into the system
                // prompt (fields are Arcs and small strings — cheap).
                let agent = Agent {
                    provider: ctx.agent.provider.clone(),
                    registry: ctx.agent.registry.clone(),
                    events: ctx.agent.events.clone(),
                    model: ctx.agent.model.clone(),
                    temperature: ctx.agent.temperature,
                    system_prompt: turn_system_prompt(&ctx.agent.system_prompt, &{
                        ctx.draft.lock().unwrap().doc.clone()
                    }),
                    max_iterations: ctx.agent.max_iterations,
                    // Reset the iteration budget on each successful edit, so a
                    // long fix-forward loop (draft → validate → run → patch)
                    // isn't cut off mid-repair the way a plain hard cap does.
                    progress_tools: super::tools::progress_tools(),
                };
                let mut history = ctx.history.lock().await;
                let pre_len = history.len();
                history.push(ChatMessage::User { content: message });
                let result = agent.run_turn(&mut history).await;
                if let Err(error) = &result {
                    // Drop the failed turn's messages so a retry starts
                    // clean — except when the turn hit the iteration cap:
                    // that history is a valid prefix of real tool work,
                    // and keeping it lets "continue" resume the turn.
                    if !keep_partial_history(error) {
                        history.truncate(pre_len);
                    }
                }
                tracing::debug!(
                    target: "workbench",
                    "agent turn took {:.1}s ({} messages in history)",
                    turn_started.elapsed().as_secs_f64(),
                    history.len()
                );
                let _ = ctx.tx.send(Msg::TurnFinished(
                    result
                        .map(|outcome| outcome.text)
                        .map_err(turn_failure_message),
                ));
            });
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
                let mut pipeline = (*ctx.pipeline).clone();
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
                let query = format!("Run the '{}' plan", doc.name);
                let result = pipeline
                    .run_explicit(&query, doc.steps.clone(), doc.finish(), Some(input))
                    .await;
                tracing::debug!(
                    target: "workbench",
                    "run took {:.1}s",
                    run_started.elapsed().as_secs_f64()
                );
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
                let tools = ctx.catalog.tools().await.unwrap_or_default();
                let shapes = ctx.store.tool_shapes().await.unwrap_or_default();
                let _ = ctx.tx.send(Msg::ContextLoaded { tools, shapes });
            });
        }

        Effect::SyncDebug { breakpoints } => {
            ctx.debug.set_breakpoints(breakpoints);
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
    let path = match &doc.path {
        Some(path) => {
            // The path is the file the draft was loaded from. Never write
            // over a file that holds a different plan — the draft's
            // identity must match the file's.
            if let Some(existing) = on_disk_identifier(path) {
                if existing != doc.identifier {
                    return Err(format!(
                        "{} holds plan '{}', not '{}' — refusing to overwrite it",
                        path.display(),
                        existing,
                        doc.identifier
                    ));
                }
            }
            path.clone()
        }
        None => {
            let dir = plans_dir
                .map(|p| p.to_path_buf())
                .ok_or_else(|| "no plans directory configured ([plans].paths)".to_string())?;
            let candidate = dir.join(format!("{}.yaml", doc.identifier));
            if candidate.exists() {
                return Err(format!(
                    "{} already exists — change the identifier or remove the file",
                    candidate.display()
                ));
            }
            candidate
        }
    };
    let yaml = serde_yaml::to_string(doc).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, yaml).map_err(|e| e.to_string())?;
    doc.path = Some(path.clone());
    guard.dirty = false;
    Ok(path.display().to_string())
}

/// The `identifier` of the plan currently in a file, if it can be read
/// and parsed at all — unreadable/garbled files return None and the save
/// proceeds (the write can't lose a plan that isn't there).
fn on_disk_identifier(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    Some(value.get("identifier")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_prompt_carries_the_current_draft() {
        let doc: PlanDoc = serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
"#,
        )
        .unwrap();
        let prompt = turn_system_prompt("BASE", &Some(doc));
        assert!(prompt.starts_with("BASE"));
        assert!(prompt.contains("identifier: demo"));
        assert!(prompt.contains("do NOT call workbench__get_plan"));

        let empty = turn_system_prompt("BASE", &None);
        assert!(empty.contains("none yet"));
    }

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
