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

pub mod doc;
pub mod exit;
pub mod plan;
mod prompts;
mod state;
#[cfg(test)]
mod tests;

pub use exit::{ExitStatus, PlanExit, EXIT_TOOL};
pub use plan::{Plan, PlannerOutput, SolverData, Step};
pub use state::{BusEntry, BusKind, RunState};

use crate::store::{Store, ToolShape};
use crate::template::{render_input, render_str, RenderError, Roots};
use crate::tools::{ToolOutcome, ToolRegistry};
use crate::EventSink;
use futures::StreamExt;
use graph_config::Role;
use graph_llm::types::{ChatMessage, ChatRequest};
use graph_llm::ModelRouter;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct Pipeline {
    pub router: Arc<ModelRouter>,
    pub registry: Arc<dyn ToolRegistry>,
    pub events: Arc<dyn EventSink>,
    /// Plan documents callable as `plan__<id>` steps (and agent tools).
    pub plans: Arc<Vec<doc::PlanDoc>>,
    /// Identifiers of plans currently executing in this call chain —
    /// cycle detection for plan-calls-plan composition.
    pub call_stack: Vec<String>,
    /// Source of observed output shapes for planner prompts. Read fresh at
    /// each planning attempt so shapes recorded earlier in the same run
    /// (agent tool calls, prior steps) are visible immediately.
    pub store: Option<Arc<dyn Store>>,
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
    #[error("plan data ran out at step {step}: {message}")]
    EmptyData { step: String, message: String },
    #[error("failed to render plan output: {0}")]
    OutputRender(String),
}

/// Maximum plan-call nesting (cycles are caught by the call stack; this
/// bounds legitimate-but-deep chains).
pub const MAX_PLAN_DEPTH: usize = 8;

/// How an explicit plan finishes.
#[derive(Debug, Clone)]
pub enum Finish {
    /// LLM synthesis of the results into prose.
    Solve(SolverData),
    /// Render a template map against the results into structured JSON.
    Render(Map<String, Value>),
    /// Side-effect plan: run the steps, produce no output.
    Silent,
}

#[derive(Debug)]
pub struct PipelineOutcome {
    pub answer: String,
    /// Structured output (Finish::Render plans, or an exit step's output).
    pub structured: Option<Value>,
    pub state: RunState,
    /// True when the answer is an error summary rather than a solution.
    pub degraded: bool,
    /// Set when an `exit` step ended the plan early.
    pub exit: Option<PlanExit>,
}

/// Result of invoking a plan (or the planner) as a callable.
pub struct PlanCall {
    pub result: Value,
    pub is_error: bool,
}

impl PlanCall {
    fn error(message: String) -> Self {
        Self {
            result: json!({"error": message}),
            is_error: true,
        }
    }
}

enum ExecutionEnd {
    Completed,
    /// An `exit` step ended the plan.
    Exited(PlanExit),
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
                self.events.replanning(state.plan_attempts);
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
                ExecutionEnd::Completed => return self.solve(state).await,
                ExecutionEnd::Exited(exit) => return Ok(self.exit_outcome(state, exit)),
                ExecutionEnd::Empty { step, message } => {
                    state.push_bus(&step, BusKind::EmptyData, message);
                    return self.solve(state).await;
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
    /// No replanning — defects are hard errors. EmptyData degrades to the
    /// solver when there is one; output/silent plans fail hard on it
    /// (automation must see that the data ran out).
    pub async fn run_explicit(
        &self,
        query: &str,
        plan: Plan,
        finish: Finish,
        input: Option<Value>,
    ) -> Result<PipelineOutcome, PipelineError> {
        let mut state = RunState {
            query: query.to_string(),
            plan,
            ..Default::default()
        };
        if let Finish::Solve(solver_data) = &finish {
            state.solver_data = solver_data.clone();
        }
        if let Some(input) = input {
            state.results.insert("input".to_string(), input);
        }

        if let Err(problems) = self.validate(&state) {
            return Err(PipelineError::InvalidPlan(problems.join("; ")));
        }
        match self.execute_all(&mut state).await {
            ExecutionEnd::Completed => self.finish(state, finish).await,
            ExecutionEnd::Exited(exit) => Ok(self.exit_outcome(state, exit)),
            ExecutionEnd::Empty { step, message } => match finish {
                Finish::Solve(_) => {
                    state.push_bus(&step, BusKind::EmptyData, message);
                    self.solve(state).await
                }
                Finish::Render(_) | Finish::Silent => {
                    Err(PipelineError::EmptyData { step, message })
                }
            },
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

    /// A nested pipeline sharing everything but one level deeper on the
    /// call stack.
    fn nested(&self, entering: Option<&str>) -> Pipeline {
        let mut child = self.clone();
        if let Some(identifier) = entering {
            child.call_stack.push(identifier.to_string());
        } else {
            child.call_stack.push("plan_and_execute".to_string());
        }
        child
    }

    fn depth_guard(&self, entering: &str) -> Option<PlanCall> {
        if self
            .call_stack
            .iter()
            .any(|frame| frame == entering && entering != "plan_and_execute")
        {
            return Some(PlanCall::error(format!(
                "recursive plan cycle: {} → {entering}",
                self.call_stack.join(" → "),
            )));
        }
        if self.call_stack.len() >= MAX_PLAN_DEPTH {
            return Some(PlanCall::error(format!(
                "plan call depth exceeded ({MAX_PLAN_DEPTH}): {}",
                self.call_stack.join(" → "),
            )));
        }
        None
    }

    /// Invoke a plan document by identifier — the shared engine behind
    /// plan tools in the agent catalog and `plan__*` steps in plans.
    /// Boxed: plans call plans, so this future is recursive.
    pub fn call_plan<'a>(
        &'a self,
        identifier: &'a str,
        mut input: Value,
    ) -> futures::future::BoxFuture<'a, PlanCall> {
        Box::pin(async move {
            if let Some(guard) = self.depth_guard(identifier) {
                return guard;
            }
            let Some(plan_doc) = self.plans.iter().find(|d| d.identifier == identifier) else {
                return PlanCall::error(format!("no plan named '{identifier}'"));
            };
            if let Some(schema) = &plan_doc.input_schema {
                doc::apply_schema_defaults(schema, &mut input);
            }
            if let Err(problems) = doc::validate_input(plan_doc, &input) {
                return PlanCall {
                    result: json!({
                        "error": "invalid or missing plan inputs",
                        "problems": problems,
                        "inputSchema": plan_doc.tool_input_schema(),
                    }),
                    is_error: true,
                };
            }
            let nested = self.nested(Some(identifier));
            let query = format!("Run the '{}' plan", plan_doc.name);
            match nested
                .run_explicit(
                    &query,
                    plan_doc.steps.clone(),
                    plan_doc.finish(),
                    Some(input),
                )
                .await
            {
                Ok(outcome) => {
                    if let Some(exit) = &outcome.exit {
                        let is_error = exit.status == ExitStatus::Error;
                        let mut result = json!({
                            "exited": true,
                            "status": if is_error { "error" } else { "success" },
                            "message": exit.message,
                        });
                        if let Some(output) = &exit.output {
                            result["output"] = Value::Object(output.clone());
                        }
                        if is_error {
                            result["error"] = json!(exit.message);
                        }
                        return PlanCall { result, is_error };
                    }
                    PlanCall {
                        result: match outcome.structured {
                            Some(structured) => structured,
                            None if outcome.answer.is_empty() => json!({
                                "ok": true,
                                "steps_executed": outcome.state.steps_executed(),
                            }),
                            None => json!({"answer": outcome.answer}),
                        },
                        is_error: false,
                    }
                }
                Err(PipelineError::EmptyData { step, message }) => PlanCall {
                    result: json!({
                        "error": format!("plan '{identifier}' had no data at step {step}: {message}"),
                        "empty_data": true,
                    }),
                    is_error: true,
                },
                Err(e) => PlanCall::error(format!("plan '{identifier}' failed: {e}")),
            }
        })
    }

    /// Invoke the free-form planner — behind `plan_and_execute` in the
    /// agent catalog and as a plan step. Boxed: recursive via plan steps.
    pub fn call_planner<'a>(
        &'a self,
        input: &'a Value,
    ) -> futures::future::BoxFuture<'a, PlanCall> {
        Box::pin(async move {
            let Some(query) = input.get("query").and_then(Value::as_str) else {
                return PlanCall::error("plan_and_execute requires a 'query' string".to_string());
            };
            if let Some(guard) = self.depth_guard("plan_and_execute") {
                return guard;
            }
            match self.nested(None).run_planned(query).await {
                Ok(outcome) => PlanCall {
                    result: json!({
                        "answer": outcome.answer,
                        "degraded": outcome.degraded,
                        "steps_executed": outcome.state.steps_executed(),
                    }),
                    is_error: false,
                },
                Err(e) => PlanCall::error(e.to_string()),
            }
        })
    }

    /// Package a triggered exit: the message is the answer, the step's
    /// output map (already rendered) is the structured output, and the
    /// solver never runs.
    fn exit_outcome(&self, mut state: RunState, exit: PlanExit) -> PipelineOutcome {
        state.push_bus(
            &exit.step.clone(),
            BusKind::Info,
            format!("exited: {}", exit.message),
        );
        PipelineOutcome {
            answer: exit.message.clone(),
            structured: exit.output.clone().map(Value::Object),
            state,
            degraded: false,
            exit: Some(exit),
        }
    }

    /// Complete an explicit run per its finish mode.
    async fn finish(
        &self,
        state: RunState,
        finish: Finish,
    ) -> Result<PipelineOutcome, PipelineError> {
        match finish {
            Finish::Solve(_) => self.solve(state).await,
            Finish::Render(output) => {
                let roots = Roots::new(&state.results);
                let mut rendered = Map::new();
                for (key, value) in &output {
                    let item = match value {
                        Value::String(template) => crate::template::render_value(template, &roots)
                            .map_err(|e| PipelineError::OutputRender(e.to_string()))?,
                        other => other.clone(),
                    };
                    rendered.insert(key.clone(), item);
                }
                Ok(PipelineOutcome {
                    answer: String::new(),
                    structured: Some(Value::Object(rendered)),
                    state,
                    degraded: false,
                    exit: None,
                })
            }
            Finish::Silent => Ok(PipelineOutcome {
                answer: String::new(),
                structured: None,
                state,
                degraded: false,
                exit: None,
            }),
        }
    }

    // ── Planner ──────────────────────────────────────────────────────────

    async fn plan_node(&self, state: &mut RunState) -> Result<(), PipelineError> {
        self.events.planning();
        let mut tools = self.registry.tools().await.unwrap_or_default();
        tools.push(exit::exit_tool_def());
        for plan_doc in self.plans.iter() {
            if self.call_stack.iter().any(|f| f == &plan_doc.identifier) {
                continue; // don't offer plans already on the call stack
            }
            tools.push(crate::tools::ToolDef {
                name: format!("plan__{}", plan_doc.identifier),
                description: plan_doc.tool_description(),
                input_schema: plan_doc.tool_input_schema(),
                output_schema: None,
                output_example: None,
                read_only: None,
            });
        }
        let shapes: HashMap<String, ToolShape> = match &self.store {
            Some(store) => store
                .tool_shapes()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|shape| (shape.tool.clone(), shape))
                .collect(),
            None => HashMap::new(),
        };
        let tools_text = prompts::describe_tools(&tools, &shapes);
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
        // Tool existence is checked at execution against the live registry.
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

            if let Some(identifier) = step.tool_name.strip_prefix("plan__") {
                self.events.tool_started(&step.tool_name, &rendered);
                let started = std::time::Instant::now();
                let call = self.call_plan(identifier, rendered).await;
                self.events
                    .tool_finished(&step.tool_name, started.elapsed(), call.is_error);
                if call.is_error {
                    return ExecutionEnd::Failed {
                        step: step.id.clone(),
                        tool: step.tool_name.clone(),
                        message: truncate(&call.result.to_string(), 2000),
                    };
                }
                state.results.insert(step.id.clone(), call.result);
                state.push_bus(&step.id, BusKind::Info, "ok");
                continue;
            }
            if step.tool_name == "plan_and_execute" {
                self.events.tool_started(&step.tool_name, &rendered);
                let started = std::time::Instant::now();
                let call = self.call_planner(&rendered).await;
                self.events
                    .tool_finished(&step.tool_name, started.elapsed(), call.is_error);
                if call.is_error {
                    return ExecutionEnd::Failed {
                        step: step.id.clone(),
                        tool: step.tool_name.clone(),
                        message: truncate(&call.result.to_string(), 2000),
                    };
                }
                state.results.insert(step.id.clone(), call.result);
                state.push_bus(&step.id, BusKind::Info, "ok");
                continue;
            }
            if step.tool_name == EXIT_TOOL {
                self.events.tool_started(EXIT_TOOL, &rendered);
                let started = std::time::Instant::now();
                let eval = exit::evaluate(&step.id, &rendered, &self.router).await;
                match eval {
                    Ok(exit::ExitEval::Passed(result)) => {
                        self.events
                            .tool_finished(EXIT_TOOL, started.elapsed(), false);
                        state.results.insert(step.id.clone(), result);
                        state.push_bus(&step.id, BusKind::Info, "gate passed");
                        continue;
                    }
                    Ok(exit::ExitEval::Exited(exit)) => {
                        self.events
                            .tool_finished(EXIT_TOOL, started.elapsed(), false);
                        return ExecutionEnd::Exited(exit);
                    }
                    Err(message) => {
                        self.events
                            .tool_finished(EXIT_TOOL, started.elapsed(), true);
                        return ExecutionEnd::Failed {
                            step: step.id.clone(),
                            tool: EXIT_TOOL.to_string(),
                            message,
                        };
                    }
                }
            }

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

    async fn solve(&self, mut state: RunState) -> Result<PipelineOutcome, PipelineError> {
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

        self.events.synthesizing();
        let mut stream = self
            .router
            .chat_stream(
                Role::Solver,
                ChatRequest {
                    system,
                    messages: vec![ChatMessage::User { content: message }],
                    ..Default::default()
                },
            )
            .await?;
        let mut response = None;
        while let Some(event) = stream.next().await {
            match event.map_err(PipelineError::Llm)? {
                graph_llm::types::StreamEvent::TextDelta(text) => self.events.solver_delta(&text),
                graph_llm::types::StreamEvent::ToolCallStarted { .. } => {}
                graph_llm::types::StreamEvent::Completed(r) => response = Some(r),
            }
        }
        let response = response.ok_or_else(|| {
            PipelineError::Llm(graph_llm::LlmError::Parse(
                "solver stream ended without completing".into(),
            ))
        })?;

        state.push_bus("solver", BusKind::Info, "answered");
        Ok(PipelineOutcome {
            answer: response.content.unwrap_or_default(),
            structured: None,
            state,
            degraded: false,
            exit: None,
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
        self.events.synthesizing();
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
            structured: None,
            state,
            degraded: true,
            exit: None,
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
