//! The plan-based execution pipeline: planner → validation → execution →
//! solver, with a bus-driven replan loop.
//!
//! Two entry points with different error policies:
//! - [`Pipeline::run_planned`] — the planner authors the plan
//!   (`plan_and_execute`). Plan defects and tool failures replan up to
//!   `max_attempts`; exhaustion degrades to an honest error summary.
//! - [`Pipeline::run_explicit`] — a human-authored plan invoked directly
//!   (plan tools, `plan run`). Never replans: defects are hard failures.
//!   In both modes, `EmptyData` (plan fine, data ran out) goes straight to
//!   the solver.

pub mod plan;
mod prompts;
mod state;
#[cfg(test)]
mod tests;

pub use plan::{Plan, PlannerOutput, SolverData, Step};
pub use state::{BusEntry, BusKind, RunState};

use crate::store::ToolShape;
use crate::template::{render_input, render_str, RenderError, Roots};
use crate::tools::{ToolOutcome, ToolRegistry};
use crate::EventSink;
use graph_config::Role;
use graph_llm::types::{ChatMessage, ChatRequest};
use graph_llm::ModelRouter;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Pipeline {
    pub router: Arc<ModelRouter>,
    pub registry: Arc<dyn ToolRegistry>,
    pub events: Arc<dyn EventSink>,
    /// Observed output shapes, keyed by tool name (feeds planner prompts).
    pub shapes: HashMap<String, ToolShape>,
    pub user_context: String,
    pub current_date: String,
    pub max_attempts: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("{0}")]
    Llm(#[from] graph_llm::LlmError),
    #[error("plan failed at step {step} ({tool}): {message}")]
    StepFailed {
        step: String,
        tool: String,
        message: String,
    },
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
}

#[derive(Debug)]
pub struct PipelineOutcome {
    pub answer: String,
    pub state: RunState,
    /// True when the answer is an error summary rather than a solution.
    pub degraded: bool,
}

enum ExecutionEnd {
    Completed,
    /// Plan defect or tool failure (replan-eligible in planned mode).
    Failed {
        step: String,
        tool: String,
        message: String,
    },
    /// Data legitimately ran out — solve with what we have.
    Empty {
        step: String,
        message: String,
    },
}

impl Pipeline {
    /// Planner-authored flow: plan, validate, execute, replan on defects,
    /// solve. Never returns Err for plan/tool problems — degrades to an
    /// error-summary answer (the caller always gets something to show).
    pub async fn run_planned(&self, query: &str) -> Result<PipelineOutcome, PipelineError> {
        let mut state = RunState {
            query: query.to_string(),
            ..Default::default()
        };

        loop {
            state.plan_attempts += 1;
            if state.plan_attempts > 1 {
                self.events.iteration(state.plan_attempts);
            }
            match self.plan_node(&mut state).await {
                Ok(()) => {}
                Err(e) => {
                    // Planner/LLM failure is not recoverable by replanning.
                    return Err(e);
                }
            }

            if let Err(problems) = self.validate(&state) {
                state.push_bus("validation", BusKind::Error, problems.join("; "));
                if state.plan_attempts >= self.max_attempts {
                    return self.error_summary(state).await;
                }
                continue;
            }

            match self.execute_all(&mut state).await {
                ExecutionEnd::Completed => return self.solve(state, false).await,
                ExecutionEnd::Empty { step, message } => {
                    state.push_bus(&step, BusKind::EmptyData, message);
                    return self.solve(state, false).await;
                }
                ExecutionEnd::Failed {
                    step,
                    tool,
                    message,
                } => {
                    state.push_bus(
                        &step,
                        BusKind::Error,
                        format!("step {step} ({tool}) failed: {message}"),
                    );
                    if state.plan_attempts >= self.max_attempts {
                        return self.error_summary(state).await;
                    }
                }
            }
        }
    }

    /// Human-authored flow: validate and execute exactly the given plan.
    /// No replanning — defects are hard errors. EmptyData still solves.
    pub async fn run_explicit(
        &self,
        query: &str,
        plan: Plan,
        solver_data: SolverData,
        input: Option<Value>,
    ) -> Result<PipelineOutcome, PipelineError> {
        let mut state = RunState {
            query: query.to_string(),
            plan,
            solver_data,
            ..Default::default()
        };
        if let Some(input) = input {
            state.results.insert("input".to_string(), input);
        }

        if let Err(problems) = self.validate(&state) {
            return Err(PipelineError::InvalidPlan(problems.join("; ")));
        }
        match self.execute_all(&mut state).await {
            ExecutionEnd::Completed => self.solve(state, false).await,
            ExecutionEnd::Empty { step, message } => {
                state.push_bus(&step, BusKind::EmptyData, message);
                self.solve(state, false).await
            }
            ExecutionEnd::Failed {
                step,
                tool,
                message,
            } => Err(PipelineError::StepFailed {
                step,
                tool,
                message,
            }),
        }
    }

    // ── Planner ──────────────────────────────────────────────────────────

    async fn plan_node(&self, state: &mut RunState) -> Result<(), PipelineError> {
        let tools = self.registry.tools().await.unwrap_or_default();
        let tools_text = prompts::describe_tools(&tools, &self.shapes);
        let executed = state.executed_steps();
        let existing_plan = if executed.is_empty() {
            "(none)".to_string()
        } else {
            serde_json::to_string_pretty(&executed).unwrap_or_default()
        };
        let step_schema = serde_json::to_string_pretty(
            &serde_json::to_value(schemars::schema_for!(Step)).unwrap_or_default(),
        )
        .unwrap_or_default();
        let last_error = state.last_error().map(|e| e.content.clone());

        let system = prompts::planner_prompt(&prompts::PlannerPromptArgs {
            current_date: &self.current_date,
            last_error: last_error.as_deref(),
            next_step_id: &state.next_step_id(),
            tools: &tools_text,
            user_context: &self.user_context,
            existing_plan: &existing_plan,
            step_schema: &step_schema,
        });

        let output: PlannerOutput = self
            .router
            .get_structured(
                Role::Planner,
                system,
                vec![ChatMessage::User {
                    content: state.query.clone(),
                }],
                "plan",
            )
            .await?;

        // Merge: executed steps are immutable; the planner's steps replace
        // the pending tail.
        let mut merged = executed;
        let executed_ids: Vec<String> = merged.iter().map(|s| s.id.clone()).collect();
        merged.extend(
            output
                .plan
                .into_iter()
                .filter(|step| !executed_ids.contains(&step.id)),
        );
        plan::sort_plan(&mut merged);
        state.plan = merged;
        state.solver_data = output.solver_data;
        plan::default_solver_data(&state.plan, &mut state.solver_data.data);
        Ok(())
    }

    // ── Validation (static, no LLM) ──────────────────────────────────────

    fn validate(&self, state: &RunState) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        if state.plan.is_empty() {
            problems.push("plan has no steps".to_string());
        }
        let known_tools: Vec<String> = self.shapes.keys().cloned().collect();
        let _ = known_tools; // tool existence checked at execution against the live registry

        let mut seen: Vec<&str> = vec!["input"];
        for step in &state.plan {
            // Template parse + reference-ordering check on every string input.
            for value in step.input.values() {
                check_templates(value, &seen, &step.id, &mut problems);
            }
            seen.push(&step.id);
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    // ── Execution ────────────────────────────────────────────────────────

    async fn execute_all(&self, state: &mut RunState) -> ExecutionEnd {
        while let Some(step) = state.next_pending_step().cloned() {
            let roots = Roots::new(&state.results);
            let rendered = match render_input(&Value::Object(step.input.clone()), &roots) {
                Ok(value) => value,
                Err(e @ RenderError::EmptyData { .. }) => {
                    return ExecutionEnd::Empty {
                        step: step.id.clone(),
                        message: e.to_string(),
                    }
                }
                Err(e) => {
                    return ExecutionEnd::Failed {
                        step: step.id.clone(),
                        tool: step.tool_name.clone(),
                        message: e.to_string(),
                    }
                }
            };

            self.events.tool_started(&step.tool_name, &rendered);
            let started = std::time::Instant::now();
            let outcome = self
                .registry
                .invoke(&step.tool_name, rendered)
                .await
                .unwrap_or_else(|e| ToolOutcome {
                    result: json!({"error": e.to_string()}),
                    is_error: true,
                });
            self.events
                .tool_finished(&step.tool_name, started.elapsed(), outcome.is_error);

            if outcome.is_error {
                return ExecutionEnd::Failed {
                    step: step.id.clone(),
                    tool: step.tool_name.clone(),
                    message: truncate(&outcome.result.to_string(), 2000),
                };
            }
            state.results.insert(step.id.clone(), outcome.result);
            state.push_bus(&step.id, BusKind::Info, "ok");
        }
        ExecutionEnd::Completed
    }

    // ── Solver ───────────────────────────────────────────────────────────

    async fn solve(
        &self,
        mut state: RunState,
        _stream: bool,
    ) -> Result<PipelineOutcome, PipelineError> {
        let roots = Roots::new(&state.results);
        // Render the solver payload; a broken solver template must not sink
        // the run — fall back to raw results.
        let mut payload = Map::new();
        for (key, value) in &state.solver_data.data {
            let rendered = match value {
                Value::String(template) => crate::template::render_value(template, &roots)
                    .unwrap_or_else(|e| Value::String(format!("(unavailable: {e})"))),
                other => other.clone(),
            };
            payload.insert(key.clone(), rendered);
        }
        if payload.is_empty() {
            payload = state.results.clone();
        }
        budget_payload(&mut payload);

        let query = if state.solver_data.query_to_answer.is_empty() {
            state.query.clone()
        } else {
            match render_str(&state.solver_data.query_to_answer, &roots) {
                Ok(rendered) => rendered,
                Err(_) => state.solver_data.query_to_answer.clone(),
            }
        };

        let mut system = String::new();
        if let Some(extra) = &state.solver_data.system_prompt {
            system.push_str(extra);
            system.push_str("\n\n");
        }
        system.push_str(prompts::SOLVER_SYSTEM_PROMPT);
        if let Some(empty) = state.bus.iter().find(|e| e.kind == BusKind::EmptyData) {
            system.push_str(&format!(
                "\n\nNote: a step's data ran out — {} — answer with what is available and say what was not found.",
                empty.content
            ));
        }
        system.push_str(&format!("\n\nCurrent date: {}", self.current_date));

        let message = format!(
            "# Task\n{query}\n\n# Collected Data\n```json\n{}\n```",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );

        let response = self
            .router
            .chat(
                Role::Solver,
                ChatRequest {
                    system,
                    messages: vec![ChatMessage::User { content: message }],
                    ..Default::default()
                },
            )
            .await?;

        state.push_bus("solver", BusKind::Info, "answered");
        Ok(PipelineOutcome {
            answer: response.content.unwrap_or_default(),
            state,
            degraded: false,
        })
    }

    /// Attempts exhausted: produce an honest failure explanation.
    async fn error_summary(&self, state: RunState) -> Result<PipelineOutcome, PipelineError> {
        let errors: Vec<String> = state
            .bus
            .iter()
            .filter(|e| e.kind == BusKind::Error)
            .map(|e| format!("- {}: {}", e.source, e.content))
            .collect();
        let plan_text = serde_json::to_string_pretty(&state.plan).unwrap_or_default();
        let message = format!(
            "# Original task\n{}\n\n# Plan attempted\n{}\n\n# Errors\n{}",
            state.query,
            plan_text,
            errors.join("\n")
        );
        let response = self
            .router
            .chat(
                Role::Solver,
                ChatRequest {
                    system: prompts::ERROR_SUMMARY_PROMPT.to_string(),
                    messages: vec![ChatMessage::User { content: message }],
                    ..Default::default()
                },
            )
            .await?;
        Ok(PipelineOutcome {
            answer: response.content.unwrap_or_default(),
            state,
            degraded: true,
        })
    }
}

/// Recursively parse template strings, recording parse failures and
/// references to steps that are not yet available at this point in the plan.
fn check_templates(value: &Value, available: &[&str], step_id: &str, problems: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if !s.contains("{{") {
                return;
            }
            match crate::template::referenced_roots(s) {
                Ok(roots) => {
                    for root in roots {
                        let is_step_ref = plan::step_number(&root).is_some();
                        if is_step_ref && !available.contains(&root.as_str()) {
                            problems.push(format!(
                                "step {step_id} references {root}, which is not an earlier step"
                            ));
                        }
                    }
                }
                Err(e) => problems.push(format!("step {step_id}: {e}")),
            }
        }
        Value::Array(items) => {
            for item in items {
                check_templates(item, available, step_id, problems);
            }
        }
        Value::Object(map) => {
            for child in map.values() {
                check_templates(child, available, step_id, problems);
            }
        }
        _ => {}
    }
}

/// Keep the solver payload within budget: truncate long strings, sample
/// long arrays. MCP results carry no verbosity annotations, so degradation
/// is generic.
fn budget_payload(payload: &mut Map<String, Value>) {
    const MAX_CHARS: usize = 400_000;
    let size = serde_json::to_string(&*payload)
        .map(|s| s.len())
        .unwrap_or(0);
    if size <= MAX_CHARS {
        return;
    }
    for value in payload.values_mut() {
        *value = shrink(value, 500, 25);
    }
}

fn shrink(value: &Value, max_string: usize, max_items: usize) -> Value {
    match value {
        Value::String(s) if s.chars().count() > max_string => Value::String(format!(
            "{}…",
            s.chars().take(max_string).collect::<String>()
        )),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(max_items)
                .map(|item| shrink(item, max_string, max_items))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), shrink(v, max_string, max_items)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}
