//! The effect executor: everything the pure reducer can't do. Long-running
//! work is spawned; completion always arrives back as a [`Msg`].

use super::app::{Effect, Msg};
use super::runner::UiGate;
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::{Pipeline, PipelineError};
use graph_core::{Agent, Store, ToolRegistry};
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
    pub draft: Arc<std::sync::Mutex<Option<PlanDoc>>>,
    /// The agent's full catalog — the context pane shows what the planner
    /// and agent can call.
    pub catalog: Arc<dyn ToolRegistry>,
    pub store: Arc<dyn Store>,
    /// Where unsaved drafts land on Ctrl+S (first configured plans dir).
    pub plans_dir: Option<PathBuf>,
    pub tx: UnboundedSender<Msg>,
}

pub fn run_effect(effect: Effect, context: &Arc<WorkbenchContext>) {
    let ctx = context.clone();
    match effect {
        Effect::RunAgentTurn { message } => {
            tokio::spawn(async move {
                let mut history = ctx.history.lock().await;
                let pre_len = history.len();
                history.push(ChatMessage::User { content: message });
                let result = ctx.agent.run_turn(&mut history).await;
                if result.is_err() {
                    // Drop the failed turn's messages so a retry starts clean.
                    history.truncate(pre_len);
                }
                let _ = ctx.tx.send(Msg::TurnFinished(
                    result.map(|_| ()).map_err(|e| e.to_string()),
                ));
            });
        }

        Effect::StartRun { gated, input } => {
            tokio::spawn(async move {
                let doc = { ctx.draft.lock().unwrap().clone() };
                let Some(doc) = doc else {
                    let _ = ctx.tx.send(Msg::RunFinished {
                        headline: "no plan to run".to_string(),
                        is_error: true,
                        results: Map::new(),
                    });
                    return;
                };
                let mut pipeline = (*ctx.pipeline).clone();
                if gated {
                    pipeline = pipeline.with_gate(Arc::new(UiGate { tx: ctx.tx.clone() }));
                }
                let query = format!("Run the '{}' plan", doc.name);
                let result = pipeline
                    .run_explicit(&query, doc.steps.clone(), doc.finish(), Some(input))
                    .await;
                let msg = match result {
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
                        Msg::RunFinished {
                            headline,
                            is_error: exited_error,
                            results: outcome.state.results,
                        }
                    }
                    Err(PipelineError::Aborted { step, state }) => Msg::RunFinished {
                        headline: format!("⊘ aborted at {step}"),
                        is_error: true,
                        results: state.results,
                    },
                    Err(error) => Msg::RunFinished {
                        headline: format!("✗ {error}"),
                        is_error: true,
                        results: Map::new(),
                    },
                };
                let _ = ctx.tx.send(msg);
            });
        }

        Effect::Validate => {
            let problems = match &*ctx.draft.lock().unwrap() {
                Some(doc) => {
                    let mut problems = ctx
                        .pipeline
                        .validate_plan(&doc.steps)
                        .err()
                        .unwrap_or_default();
                    if let Err(problem) = graph_core::pipeline::doc::validate_doc(doc) {
                        if !problems.contains(&problem) {
                            problems.push(problem);
                        }
                    }
                    problems
                }
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

        Effect::SavePlan => {
            let result = save_draft(&ctx);
            let _ = ctx.tx.send(Msg::Saved(result));
        }
    }
}

fn save_draft(ctx: &WorkbenchContext) -> Result<String, String> {
    let mut guard = ctx.draft.lock().unwrap();
    let Some(doc) = guard.as_mut() else {
        return Err("no draft".to_string());
    };
    let path = match &doc.path {
        Some(path) => path.clone(),
        None => {
            let dir = ctx
                .plans_dir
                .clone()
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
    Ok(path.display().to_string())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}
