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

pub mod agent;
pub mod ask;
pub mod authoring;
pub mod body;
pub mod catalog;
pub mod condition;
pub mod decision;
pub mod doc;
pub mod exit;
pub mod filter;
pub mod gate;
pub mod interlocutor;
pub mod iterate;
mod outline;
pub mod plan;
mod prompts;
mod state;
#[cfg(test)]
mod tests;

pub use agent::{agent_tool_def, AGENT_TOOL};
pub use ask::{ask_tool_def, AskResult, WhenUnanswered, ASK_TOOL};
pub use authoring::{EditAccepted, EditRejected, WriteError};
pub use catalog::{CatalogCheck, ToolCatalog};
pub use decision::DECIDE_TOOL;
pub use exit::{ExitStatus, PlanExit, EXIT_TOOL};
pub use filter::FILTER_TOOL;
pub use gate::{ErrorDecision, ExecutionGate, GateContext, GateDecision, StepPath};
pub use interlocutor::{AskOutcome, AskRequest, Interlocutor};
pub use iterate::{MAP_TOOL, REDUCE_TOOL};
pub use outline::{OutlineItem, PlanOutline, StepDraft, MAX_STEP_ATTEMPTS};
pub use plan::{Plan, PlannerOutput, SolverData, Step};
pub use prompts::CONTROL_STEP_RULES;
pub use state::{BusEntry, BusKind, RunState};

use crate::store::{Store, ToolShape};
use crate::template::{render_input, render_str, RenderError, Roots};
use crate::tools::{ToolOutcome, ToolRegistry};
use crate::usage::CallSite;
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
    /// Optional interactive hook consulted before every real tool dispatch
    /// (see [`gate`] module docs for scope). Propagates into nested plan
    /// calls via [`Pipeline::nested`].
    pub gate: Option<Arc<dyn ExecutionGate>>,
    /// Optional way to put a question to a human, used by `ask` steps.
    /// `None` means nobody can be asked — every `ask` resolves as
    /// [`interlocutor::AskOutcome::Unavailable`] and the step's declared
    /// `whenUnanswered` decides what happens. Propagates into nested plan
    /// calls via [`Pipeline::nested`].
    pub interlocutor: Option<Arc<dyn Interlocutor>>,
    /// The loadable-tool catalog, when the caller can build one (the CLI
    /// runtime does; bare test pipelines don't). When present,
    /// [`Pipeline::run_explicit`] resolves every step tool against it and
    /// fails before the first step instead of mid-run. Planner-authored
    /// runs ([`Pipeline::run_planned`]) are exempt: the planner draws from
    /// the live registry, and execution + replanning already handle its
    /// hallucinations.
    pub catalog: Option<Arc<ToolCatalog>>,
    pub user_context: String,
    pub current_date: String,
    pub max_attempts: u32,
    /// Where this run's model calls are tallied. Shared with the
    /// [`ModelRouter`] as its `UsageMeter`, and — because [`Pipeline::nested`]
    /// clones — with every plan this one calls, so a composed plan's spend
    /// rolls up into its caller for free.
    ///
    /// Deliberately **not** drained by the pipeline, and so deliberately not
    /// a field on [`PipelineOutcome`]: `call_plan` runs a nested plan through
    /// the same `run_explicit` entry point against this same ledger, so a
    /// drain in there would hand the inner plan its caller's tally and leave
    /// the caller reporting only what happened afterwards. The owner of the
    /// top-level run calls [`UsageLedger::take`] once, when it is over.
    pub usage: Arc<crate::usage::UsageLedger>,
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
    /// An [`ExecutionGate`] ended the run. Carries the partial run state —
    /// results collected before the abort — so interactive callers can
    /// render what happened. Never replans, never degrades.
    #[error("run aborted at step {step}")]
    Aborted {
        step: String,
        /// The failing tool's error result when the abort was triggered by a
        /// tool error (`None` for a plain breakpoint/continue abort). Lets
        /// interactive callers surface why the step failed.
        error: Option<Value>,
        state: Box<RunState>,
    },
    /// Drafting could not produce a valid step within its
    /// budget. Carries the valid partial draft — everything accepted
    /// before the failure — so interactive callers can salvage it
    /// (mirrors `Aborted` carrying the partial run state).
    #[error("plan drafting stopped at step {step_id}: {}", problems.join("; "))]
    DraftStepExhausted {
        step_id: String,
        /// Failed attempts for the step (0 when the step budget, not the
        /// per-step retry budget, ran out).
        attempts: u32,
        problems: Vec<String>,
        partial: Box<PlannerOutput>,
    },
}

/// Every control step, described the way the planner sees it.
///
/// The single source of truth for the control-step vocabulary: the
/// planner's catalog is built from it, and so is the `tools` command
/// surface (`graph tools list`/`show`, and `graph_tools_*` over MCP). That
/// matters more than it looks — a control step that is not in a catalog an
/// agent can query is a step it can only learn about by reading graph's
/// source, which is exactly what agents did before this existed.
///
/// These are *descriptions*, not registry entries: control steps are
/// evaluated by the executor and never dispatched, so they are deliberately
/// absent from every [`ToolRegistry`](crate::tools::ToolRegistry) and
/// cannot be invoked.
pub fn control_step_defs() -> Vec<crate::tools::ToolDef> {
    vec![
        agent::agent_tool_def(),
        ask::ask_tool_def(),
        exit::exit_tool_def(),
        decision::decide_tool_def(),
        filter::filter_tool_def(),
        iterate::map_tool_def(),
        iterate::reduce_tool_def(),
    ]
}

/// True when `name` is a control step — executor-evaluated step vocabulary
/// rather than an invokable tool.
pub fn is_control_step(name: &str) -> bool {
    matches!(
        name,
        AGENT_TOOL | ASK_TOOL | EXIT_TOOL | DECIDE_TOOL | FILTER_TOOL | MAP_TOOL | REDUCE_TOOL
    )
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
    /// True when an [`ExecutionGate`] aborted the nested run — propagated
    /// so the abort stays hard instead of degrading into a replannable
    /// tool error.
    pub aborted: bool,
}

impl PlanCall {
    fn error(message: String) -> Self {
        Self {
            result: json!({"error": message}),
            is_error: true,
            aborted: false,
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
    /// An [`ExecutionGate`] ended the run — hard stop, never replans.
    Aborted {
        step: String,
        /// The failing tool's error result when the abort was triggered by a
        /// tool error (`None` for a plain breakpoint/continue abort).
        error: Option<Value>,
    },
}

/// How a dispatched tool call failed.
enum DispatchError {
    /// Tool failure — the (truncated) failure message.
    Failed(String),
    /// The gate aborted the run. Carries the failing tool's error result
    /// (when the abort was triggered by a tool error) so interactive
    /// callers can show *why* the step failed, not just that it aborted.
    Aborted { error: Option<Value> },
}

impl Pipeline {
    /// Install an [`ExecutionGate`], consulted before every real tool
    /// dispatch in this pipeline and any plans it calls.
    pub fn with_gate(mut self, gate: Arc<dyn ExecutionGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Install an [`Interlocutor`], letting `ask` steps in this pipeline
    /// and any plans it calls reach a human.
    pub fn with_interlocutor(mut self, interlocutor: Arc<dyn Interlocutor>) -> Self {
        self.interlocutor = Some(interlocutor);
        self
    }

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
                ExecutionEnd::Aborted { step, error } => {
                    return Err(PipelineError::Aborted {
                        step,
                        error,
                        state: Box::new(state),
                    })
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

        let mut problems = self.validate(&state).err().unwrap_or_default();
        // Second layer: catalog-aware tool resolution, so an unresolvable
        // tool fails here — before any step spends tool calls or LLM money.
        if let Some(catalog) = &self.catalog {
            problems.extend(catalog::resolve_step_tools(&state.plan, catalog));
        }
        if !problems.is_empty() {
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
            ExecutionEnd::Aborted { step, error } => Err(PipelineError::Aborted {
                step,
                error,
                state: Box::new(state),
            }),
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
                    aborted: false,
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
                        return PlanCall {
                            result,
                            is_error,
                            aborted: false,
                        };
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
                        aborted: false,
                    }
                }
                Err(PipelineError::EmptyData { step, message }) => PlanCall {
                    result: json!({
                        "error": format!("plan '{identifier}' had no data at step {step}: {message}"),
                        "empty_data": true,
                    }),
                    is_error: true,
                    aborted: false,
                },
                Err(PipelineError::Aborted { step, error, .. }) => PlanCall {
                    result: json!({
                        "error": format!("plan '{identifier}' aborted at step {step}"),
                        "cause": error,
                    }),
                    is_error: true,
                    aborted: true,
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
                    aborted: false,
                },
                Err(PipelineError::Aborted { step, error, .. }) => PlanCall {
                    result: json!({
                        "error": format!("planned run aborted at step {step}"),
                        "cause": error,
                    }),
                    is_error: true,
                    aborted: true,
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

    /// Gather the planner-facing tool catalog shared by every system-prompt
    /// builder: registry tools, the control-step defs (exit/decide/filter/
    /// map/reduce, in that order), and `plan__*` docs for plans not already on
    /// the call stack, described against the shape cache (read fresh here so
    /// each planning attempt sees the latest observed shapes). Returns the
    /// described-tools text and the pretty-printed step schema.
    async fn planner_catalog(&self) -> (String, String) {
        let mut tools = self.registry.tools().await.unwrap_or_default();
        tools.extend(control_step_defs());
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
        let step_schema = serde_json::to_string_pretty(
            &serde_json::to_value(schemars::schema_for!(Step)).unwrap_or_default(),
        )
        .unwrap_or_default();
        (tools_text, step_schema)
    }

    /// The planner's system prompt: tool catalog (registry, control steps,
    /// callable plans), observed shapes, and templating contract.
    /// `existing_plan` is executed-and-immutable steps (the replan
    /// continuation); drafting a fresh plan goes through
    /// [`Pipeline::draft_plan`] and its own prompt instead.
    async fn planner_system(
        &self,
        existing_plan: &str,
        next_step_id: &str,
        last_error: Option<&str>,
    ) -> String {
        let (tools_text, step_schema) = self.planner_catalog().await;

        prompts::planner_prompt(&prompts::PlannerPromptArgs {
            current_date: &self.current_date,
            last_error,
            next_step_id,
            tools: &tools_text,
            user_context: &self.user_context,
            existing_plan,
            step_schema: &step_schema,
        })
    }

    async fn plan_node(&self, state: &mut RunState) -> Result<(), PipelineError> {
        self.events.planning();
        let executed = state.executed_steps();
        let existing_plan = if executed.is_empty() {
            "(none)".to_string()
        } else {
            serde_json::to_string_pretty(&executed).unwrap_or_default()
        };
        let last_error = state.last_error().map(|e| e.content.clone());
        let system = self
            .planner_system(&existing_plan, &state.next_step_id(), last_error.as_deref())
            .await;

        let output: PlannerOutput = CallSite::role("planner")
            .in_plans(&self.call_stack)
            .scope(self.router.get_structured(
                Role::Planner,
                system,
                vec![ChatMessage::User {
                    content: state.query.clone(),
                }],
                "plan",
            ))
            .await?;

        // Merge: executed steps are immutable and keep their run order;
        // the planner's steps replace the pending tail in emitted order.
        let mut merged = executed;
        let executed_ids: Vec<String> = merged.iter().map(|s| s.id.clone()).collect();
        merged.extend(
            output
                .plan
                .into_iter()
                .filter(|step| !executed_ids.contains(&step.id)),
        );
        state.plan = merged;
        state.solver_data = output.solver_data;
        plan::default_solver_data(&state.plan, &mut state.solver_data.data);
        Ok(())
    }

    // ── Validation (static, no LLM) ──────────────────────────────────────

    fn validate(&self, state: &RunState) -> Result<(), Vec<String>> {
        self.validate_plan(&state.plan)
    }

    /// Static validation of a plan: template parse, reference ordering,
    /// control-step gates and bodies. No LLM, no registry — tool existence
    /// is checked at execution. Returns every problem found, not just the
    /// first.
    ///
    /// The rules live in [`authoring::validate_steps`] so plan authoring can
    /// validate an edit without constructing a pipeline.
    pub fn validate_plan(&self, plan: &Plan) -> Result<(), Vec<String>> {
        let problems = authoring::validate_steps(plan);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    // ── Execution ────────────────────────────────────────────────────────

    async fn execute_all(&self, state: &mut RunState) -> ExecutionEnd {
        while let Some(step) = state.next_pending_step().cloned() {
            // Control steps defer rendering: only their gate/list renders
            // up front, and bodies render lazily — per chosen branch
            // (decide) or per item (map/reduce). Their step events carry
            // the raw input for the same reason.
            let control = match step.tool_name.as_str() {
                AGENT_TOOL | ASK_TOOL | DECIDE_TOOL | FILTER_TOOL | MAP_TOOL | REDUCE_TOOL => {
                    self.events.step_started(
                        &self.call_stack,
                        &step.id,
                        &step.tool_name,
                        &Value::Object(step.input.clone()),
                    );
                    let started = std::time::Instant::now();
                    let run = match step.tool_name.as_str() {
                        AGENT_TOOL => self.run_agent(&step, state).await,
                        ASK_TOOL => self.run_ask(&step, state).await,
                        DECIDE_TOOL => self.run_decide(&step, state).await,
                        FILTER_TOOL => self.run_filter(&step, state).await,
                        MAP_TOOL => self.run_map(&step, state).await,
                        _ => self.run_reduce(&step, state).await,
                    };
                    match &run {
                        Ok(result) => self.events.step_finished(
                            &self.call_stack,
                            &step.id,
                            &step.tool_name,
                            result,
                            false,
                            started.elapsed(),
                        ),
                        Err(ExecutionEnd::Failed { message, .. }) => self.events.step_finished(
                            &self.call_stack,
                            &step.id,
                            &step.tool_name,
                            &json!({"error": message}),
                            true,
                            started.elapsed(),
                        ),
                        Err(ExecutionEnd::Empty { message, .. }) => self.events.step_finished(
                            &self.call_stack,
                            &step.id,
                            &step.tool_name,
                            &json!({"error": message, "emptyData": true}),
                            true,
                            started.elapsed(),
                        ),
                        Err(_) => {}
                    }
                    Some(run)
                }
                _ => None,
            };
            if let Some(run) = control {
                match run {
                    Ok(result) => {
                        state.results.insert(step.id.clone(), result);
                        continue;
                    }
                    Err(end) => return end,
                }
            }

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

            if step.tool_name == EXIT_TOOL {
                self.events
                    .step_started(&self.call_stack, &step.id, EXIT_TOOL, &rendered);
                self.events.tool_started(EXIT_TOOL, &rendered);
                let started = std::time::Instant::now();
                let eval = CallSite::role("judge")
                    .at(&step.id)
                    .in_plans(&self.call_stack)
                    .scope(exit::evaluate(&step.id, &rendered, &self.router))
                    .await;
                match eval {
                    Ok(exit::ExitEval::Passed(result)) => {
                        self.events
                            .tool_finished(EXIT_TOOL, started.elapsed(), false);
                        self.events.step_finished(
                            &self.call_stack,
                            &step.id,
                            EXIT_TOOL,
                            &result,
                            false,
                            started.elapsed(),
                        );
                        state.results.insert(step.id.clone(), result);
                        state.push_bus(&step.id, BusKind::Info, "gate passed");
                        continue;
                    }
                    Ok(exit::ExitEval::Exited(exit)) => {
                        self.events
                            .tool_finished(EXIT_TOOL, started.elapsed(), false);
                        self.events.step_finished(
                            &self.call_stack,
                            &step.id,
                            EXIT_TOOL,
                            &serde_json::to_value(&exit).unwrap_or_default(),
                            false,
                            started.elapsed(),
                        );
                        return ExecutionEnd::Exited(exit);
                    }
                    Err(message) => {
                        self.events
                            .tool_finished(EXIT_TOOL, started.elapsed(), true);
                        self.events.step_finished(
                            &self.call_stack,
                            &step.id,
                            EXIT_TOOL,
                            &json!({"error": message}),
                            true,
                            started.elapsed(),
                        );
                        return ExecutionEnd::Failed {
                            step: step.id.clone(),
                            tool: EXIT_TOOL.to_string(),
                            message,
                        };
                    }
                }
            }

            let path = StepPath::top(&step.id);
            match self
                .dispatch(&path, &step.tool_name, rendered, &state.results)
                .await
            {
                Ok(result) => {
                    state.results.insert(step.id.clone(), result);
                    state.push_bus(&step.id, BusKind::Info, "ok");
                }
                Err(DispatchError::Aborted { error }) => {
                    return ExecutionEnd::Aborted {
                        step: step.id.clone(),
                        error,
                    }
                }
                Err(DispatchError::Failed(message)) => {
                    return ExecutionEnd::Failed {
                        step: step.id.clone(),
                        tool: step.tool_name.clone(),
                        message,
                    }
                }
            }
        }
        ExecutionEnd::Completed
    }

    /// Route one rendered tool call — a `plan__*` step, `plan_and_execute`,
    /// or the registry — consulting the gate and emitting tool and step
    /// events. Every real tool call at any depth funnels through here.
    /// `scope` is the map the input rendered against, handed to the gate
    /// as the debugger's locals.
    async fn dispatch(
        &self,
        path: &StepPath,
        tool_name: &str,
        rendered: Value,
        scope: &Map<String, Value>,
    ) -> Result<Value, DispatchError> {
        let path_text = path.to_string();
        // The input is moved into the invocation; keep a copy for the
        // error-consult context (gated runs only).
        let rendered_snapshot = self.gate.as_ref().map(|_| rendered.clone());
        if let Some(gate) = &self.gate {
            let decision = gate
                .before_tool(GateContext {
                    path,
                    tool_name,
                    rendered_input: &rendered,
                    call_stack: &self.call_stack,
                    scope,
                })
                .await;
            match decision {
                GateDecision::Proceed => {}
                GateDecision::Skip { result } => {
                    // No tool ran — no tool events — but the step has a
                    // value, so sinks still see it finish.
                    self.events.step_finished(
                        &self.call_stack,
                        &path_text,
                        tool_name,
                        &result,
                        false,
                        std::time::Duration::ZERO,
                    );
                    return Ok(result);
                }
                GateDecision::Abort => return Err(DispatchError::Aborted { error: None }),
            }
        }

        self.events
            .step_started(&self.call_stack, &path_text, tool_name, &rendered);
        self.events.tool_started(tool_name, &rendered);
        let started = std::time::Instant::now();
        let outcome = if let Some(identifier) = tool_name.strip_prefix("plan__") {
            let call = self.call_plan(identifier, rendered).await;
            if call.aborted {
                // A nested abort is already a gate decision — never re-ask.
                return Err(DispatchError::Aborted {
                    error: Some(call.result),
                });
            }
            ToolOutcome {
                result: call.result,
                is_error: call.is_error,
            }
        } else if tool_name == "plan_and_execute" {
            let call = self.call_planner(&rendered).await;
            if call.aborted {
                return Err(DispatchError::Aborted {
                    error: Some(call.result),
                });
            }
            ToolOutcome {
                result: call.result,
                is_error: call.is_error,
            }
        } else {
            // Prompt tools (`builtin__infer`, user tools with a `prompt`)
            // make model calls from inside the registry, where there is no
            // step context. Scoping the dispatch is what attributes them —
            // and it covers every prompt tool at every depth at once, rather
            // than one edit per tool implementation. Tools that never call a
            // model just carry an unused scope.
            //
            // The `plan__`/`plan_and_execute` branches above are deliberately
            // left unscoped: a nested plan sets its own scopes at its own
            // inference sites, which is what keeps its steps distinct.
            CallSite::role(tool_name)
                .at(&path_text)
                .in_plans(&self.call_stack)
                .scope(self.registry.invoke(tool_name, rendered))
                .await
                .unwrap_or_else(|e| ToolOutcome {
                    result: json!({"error": e.to_string()}),
                    is_error: true,
                })
        };
        // tool_finished always describes the real call; when a gate
        // replaces an error below, step_finished carries the resolution.
        self.events
            .tool_finished(tool_name, started.elapsed(), outcome.is_error);

        if outcome.is_error {
            if let (Some(gate), Some(snapshot)) = (&self.gate, &rendered_snapshot) {
                let decision = gate
                    .on_tool_error(
                        GateContext {
                            path,
                            tool_name,
                            rendered_input: snapshot,
                            call_stack: &self.call_stack,
                            scope,
                        },
                        &outcome.result,
                    )
                    .await;
                match decision {
                    ErrorDecision::Fail => {}
                    ErrorDecision::Replace { result } => {
                        self.events.step_finished(
                            &self.call_stack,
                            &path_text,
                            tool_name,
                            &result,
                            false,
                            started.elapsed(),
                        );
                        return Ok(result);
                    }
                    ErrorDecision::Abort => {
                        self.events.step_finished(
                            &self.call_stack,
                            &path_text,
                            tool_name,
                            &outcome.result,
                            true,
                            started.elapsed(),
                        );
                        return Err(DispatchError::Aborted {
                            error: Some(outcome.result),
                        });
                    }
                }
            }
            self.events.step_finished(
                &self.call_stack,
                &path_text,
                tool_name,
                &outcome.result,
                true,
                started.elapsed(),
            );
            return Err(DispatchError::Failed(truncate(
                &outcome.result.to_string(),
                2000,
            )));
        }
        self.events.step_finished(
            &self.call_stack,
            &path_text,
            tool_name,
            &outcome.result,
            false,
            started.elapsed(),
        );
        Ok(outcome.result)
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
        // The scope covers draining the stream, not just opening it: usage
        // rides the terminal event, so a scope that ended at `chat_stream`
        // would meter the tokens with no idea who spent them.
        let response = CallSite::role("solver")
            .in_plans(&self.call_stack)
            .scope(async {
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
                        graph_llm::types::StreamEvent::TextDelta(text) => {
                            self.events.solver_delta(&text)
                        }
                        graph_llm::types::StreamEvent::ToolCallStarted { .. } => {}
                        graph_llm::types::StreamEvent::Completed(r) => response = Some(r),
                    }
                }
                Ok::<_, PipelineError>(response)
            })
            .await?;
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
        let response = CallSite::role("solver")
            .in_plans(&self.call_stack)
            .scope(self.router.chat(
                Role::Solver,
                ChatRequest {
                    system: prompts::ERROR_SUMMARY_PROMPT.to_string(),
                    messages: vec![ChatMessage::User { content: message }],
                    ..Default::default()
                },
            ))
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
/// references to roots that are not available at this point in the plan —
/// `available` carries `input`, every earlier step id, and (inside
/// control-step bodies) the pseudo-roots and earlier same-body ids.
fn check_templates(value: &Value, available: &[&str], step_id: &str, problems: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if !s.contains("{{") {
                return;
            }
            match crate::template::referenced_roots(s) {
                Ok(roots) => {
                    for root in roots {
                        if !available.contains(&root.as_str()) {
                            problems.push(format!(
                                "step {step_id} references {root}, which is not \
                                 `input` or an earlier step"
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
