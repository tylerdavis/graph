use super::plan::{self, Plan, PlannerOutput, SolverData, Step};
use super::{prompts, Pipeline};
use crate::tools::ToolDef;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const DRAFT_CONTEXT_TOOL: &str = "builtin__draft_context";

pub const ACCEPT_STEP_TOOL: &str = "builtin__accept_step";

pub const AUTHOR_PLAN: &str = "author_plan";

/// One step-drafting response.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StepDraft {
    #[schemars(
        description = "The next step's id, as requested. Null, with toolName and input, when the accepted steps already complete the plan."
    )]
    #[serde(default)]
    pub id: Option<String>,
    #[schemars(
        description = "Exact tool name from the tool list, e.g. \"linear__search_issues\"."
    )]
    #[serde(default)]
    pub tool_name: Option<String>,
    #[schemars(
        description = "Tool input. String values may reference earlier steps with templates like {{E0.values.0.id}}."
    )]
    #[serde(default)]
    pub input: Option<Map<String, Value>>,
    #[schemars(description = "Why this step exists and what it should produce.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[schemars(
        description = "True when the plan is complete: after this step, or already (no step)."
    )]
    #[serde(default)]
    pub plan_complete: bool,
    #[schemars(
        description = "The question the solver must answer; always includes the user's original task. Set it on the first step when the plan finishes with a solver; omit it for a plan that finishes with an `output` map or exists only for its side effects."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_to_answer: Option<String>,
    #[schemars(description = "Extra system-prompt guidance for the solver (optional).")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

impl StepDraft {
    fn step(&self) -> Result<Option<Step>, String> {
        match (&self.id, &self.tool_name, &self.input) {
            (None, None, None) => Ok(None),
            (Some(id), Some(tool_name), input) => Ok(Some(Step {
                id: id.clone(),
                tool_name: tool_name.clone(),
                input: input.clone().unwrap_or_default(),
                reasoning: self.reasoning.clone(),
            })),
            _ => Err("a step needs both id and toolName".to_string()),
        }
    }
}

/// Per-step retry budget: attempts at producing a step that passes static
/// validation before drafting gives up with the valid partial. Exported
/// (via the pipeline module) so the workbench can label attempts against
/// the same ceiling without hardcoding it.
pub const MAX_STEP_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DraftSolver {
    pub query_to_answer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DraftState {
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub solver: Option<DraftSolver>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub problems: Vec<String>,
    #[serde(default)]
    pub settled: bool,
    #[serde(default)]
    pub correction: String,
}

pub struct Draft {
    pub output: PlannerOutput,
    pub failed_step: Option<String>,
    pub problems: Vec<String>,
}

impl Draft {
    pub fn from_authored(result: &Value) -> Result<Self, String> {
        let doc = super::native_tools::plan_doc(&result["plan"])?;
        Ok(Self {
            output: PlannerOutput {
                plan: doc.steps,
                solver_data: doc.solver,
            },
            failed_step: result["failed_step"].as_str().map(str::to_string),
            problems: serde_json::from_value(result["drafting_problems"].clone())
                .unwrap_or_default(),
        })
    }

    pub fn from_expanded(result: &Value) -> Result<Self, String> {
        let state: DraftState = serde_json::from_value(result.clone())
            .map_err(|e| format!("the draft plan returned an unexpected shape: {e}"))?;
        Ok(Self {
            failed_step: state.failed.then(|| next_step_id(&state.steps)),
            output: assemble_output(&state),
            problems: state.problems,
        })
    }
}

pub fn draft_input(goal: &str) -> Value {
    json!({ "goal": goal })
}

#[derive(Debug, Deserialize)]
struct DraftContextInput {
    goal: String,
    outline: Vec<String>,
    #[serde(default)]
    entry: String,
    #[serde(default)]
    state: Option<DraftState>,
    #[serde(default)]
    tools: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AcceptStepInput {
    state: DraftState,
    draft: Value,
    attempt: u32,
}

pub fn draft_context_tool_def() -> ToolDef {
    ToolDef {
        name: DRAFT_CONTEXT_TOOL.to_string(),
        description: "Prepares drafting the next plan step for one outline entry: the drafting \
                      state for this entry, the step request, the attempt numbers, the step \
                      response schema, and the catalog, rules and context the drafting prompt \
                      is built from. Once the draft is done or failed, the state comes back \
                      settled and no step is requested."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["goal", "outline", "state"],
            "properties": {
                "goal": {"type": "string", "description": "What the plan should accomplish"},
                "outline": {"type": "array", "items": {"type": "string"}, "description": "The whole outline, in order"},
                "entry": {"type": "string", "description": "The outline entry this step advances; empty to finish the plan"},
                "state": {"type": "object", "description": "The drafting state so far"},
                "tools": {"type": "array", "description": "The tools to offer this step, as names or the `tools` builtin__search_tools returned; omitted means the whole catalog. Always-loaded tools and tools earlier steps used are always offered too"}
            }
        }),
        output_schema: None,
        output_example: None,
        read_only: Some(true),
    }
}

pub fn accept_step_tool_def() -> ToolDef {
    ToolDef {
        name: ACCEPT_STEP_TOOL.to_string(),
        description: "Validates one drafted step against the steps already accepted. A valid step \
                      is accepted and the state settles; an invalid one leaves the state \
                      unsettled with its problems and a correction for the next attempt, and \
                      fails the draft on the last attempt."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["state", "draft", "attempt"],
            "properties": {
                "state": {"type": "object", "description": "The drafting state for this entry"},
                "draft": {"type": "object", "description": "The step response: step, planComplete, queryToAnswer, systemPrompt"},
                "attempt": {"type": "integer", "description": "This attempt's number, from 1"}
            }
        }),
        output_schema: None,
        output_example: None,
        read_only: Some(true),
    }
}

impl Pipeline {
    pub(super) async fn draft_context(&self, input: Value) -> Result<Value, String> {
        let input: DraftContextInput = serde_json::from_value(input)
            .map_err(|e| format!("invalid draft_context input: {e}"))?;
        if input.outline.iter().all(|entry| entry.trim().is_empty()) {
            return Err("the outline has no entries".to_string());
        }
        let mut state = input.state.unwrap_or_default();
        state.settled = state.done || state.failed;
        state.correction = String::new();
        let entry = input.entry.trim();
        let next_step_id = next_step_id(&state.steps);
        let (standing_tools, tools, step_schema) = if state.settled {
            (String::new(), String::new(), String::new())
        } else {
            let index = state.steps.len();
            if index == 0 && input.outline.first().map(|first| first.trim()) == Some(entry) {
                self.events.draft_outline(&json!(input.outline));
            }
            let summary = if entry.is_empty() {
                "finalize the plan"
            } else {
                entry
            };
            self.events.draft_step_started(index, summary);
            match &input.tools {
                Some(tools) => {
                    let mut scope: std::collections::BTreeSet<String> =
                        super::search::names_of(tools).into_iter().collect();
                    scope.extend(super::native_tools::called_tools(&json!(state.steps)));
                    self.drafting_catalog(&scope).await
                }
                None => {
                    let (whole, step_schema) = self.planner_catalog().await;
                    (
                        whole,
                        "(none beyond the tools above)".to_string(),
                        step_schema,
                    )
                }
            }
        };
        Ok(json!({
            "request": step_request_content(&input.goal, &input.outline, &state.steps, entry, &next_step_id),
            "state": state,
            "attempts": (1..=MAX_STEP_ATTEMPTS).collect::<Vec<_>>(),
            "draft_schema": step_draft_schema(),
            "standing_tools": standing_tools,
            "tools": tools,
            "step_schema": step_schema,
            "date": self.current_date,
            "user": self.user_context,
            "templating_rules": prompts::TEMPLATING_RULES,
            "planning_rules": prompts::PLANNING_RULES,
            "control_step_rules": prompts::CONTROL_STEP_RULES,
        }))
    }

    fn catalog_problems(&self, steps: &Plan) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let chat = format!(
            "{}{}",
            super::AGENT_TOOL_PREFIX,
            crate::agent::doc::CHAT_AGENT
        );
        if super::native_tools::called_tools(&json!(steps)).contains(&chat) {
            errors.push(format!(
                "{chat} is not available to plans; call the tools the step needs, or use an `agent` step with an explicit tool list"
            ));
        }
        if let Some(catalog) = self.catalog.as_deref() {
            let doc = super::doc::PlanDoc {
                identifier: "draft".to_string(),
                name: "draft".to_string(),
                description: String::new(),
                exemplars: Vec::new(),
                requires_servers: Vec::new(),
                input_schema: None,
                steps: steps.clone(),
                solver: None,
                output: None,
                path: None,
            };
            errors
                .extend(super::catalog::resolve_plan_tools_deep(&doc, &self.plans, catalog).errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub(super) fn accept_step(&self, input: Value) -> Result<Value, String> {
        let AcceptStepInput {
            mut state,
            draft,
            attempt,
        } = serde_json::from_value(input).map_err(|e| format!("invalid accept_step input: {e}"))?;
        if state.settled {
            return Ok(json!(state));
        }
        let index = state.steps.len();
        let next_step_id = next_step_id(&state.steps);
        let draft = match read_draft(draft) {
            Ok(draft) => draft,
            Err((raw, problems)) => {
                self.events
                    .draft_step_finished(index, &raw, &problems, attempt);
                state.correction = format!(
                    "Your previous response was:\n{raw}\n\nIt does not match the response schema:\n- {}\nProduce step {next_step_id} again as one object with id, toolName, input, reasoning and planComplete.",
                    problems.join("\n- ")
                );
                state.problems = problems;
                if attempt >= MAX_STEP_ATTEMPTS {
                    state.failed = true;
                    state.settled = true;
                }
                return Ok(json!(state));
            }
        };
        let step = match draft.step() {
            Ok(step) => step,
            Err(problem) => {
                let problems = vec![problem];
                self.events
                    .draft_step_finished(index, &json!(draft), &problems, attempt);
                state.correction = correction(&draft, &problems, &next_step_id);
                state.problems = problems;
                if attempt >= MAX_STEP_ATTEMPTS {
                    state.failed = true;
                    state.settled = true;
                }
                return Ok(json!(state));
            }
        };
        let problems = match step {
            None if draft.plan_complete && !state.steps.is_empty() => {
                absorb(&mut state, &draft);
                state.done = true;
                return Ok(json!(settle(state)));
            }
            None => {
                let problems = vec!["produced no step for an incomplete plan".to_string()];
                self.events
                    .draft_step_finished(index, &Value::Null, &problems, attempt);
                problems
            }
            Some(step) => {
                let mut candidate = state.steps.clone();
                candidate.push(step.clone());
                match self
                    .validate_plan(&candidate)
                    .and_then(|()| self.catalog_problems(&candidate))
                {
                    Ok(()) => {
                        self.events
                            .draft_step_finished(index, &json!(step), &[], attempt);
                        state.steps = candidate;
                        absorb(&mut state, &draft);
                        state.done = draft.plan_complete;
                        return Ok(json!(settle(state)));
                    }
                    Err(problems) => {
                        self.events
                            .draft_step_finished(index, &json!(step), &problems, attempt);
                        problems
                    }
                }
            }
        };
        state.correction = correction(&draft, &problems, &next_step_id);
        state.problems = problems;
        if attempt >= MAX_STEP_ATTEMPTS {
            state.failed = true;
            state.settled = true;
        }
        Ok(json!(state))
    }
}

fn step_draft_schema() -> Value {
    let mut schema = json!(schemars::schema_for!(StepDraft));
    schema["required"] = json!(["id", "toolName", "input", "planComplete"]);
    schema["additionalProperties"] = json!(false);
    schema
}

fn read_draft(raw: Value) -> Result<StepDraft, (Value, Vec<String>)> {
    let schema = step_draft_schema();
    let raw = crate::user_tools::coerce_to_schema(raw, &schema);
    let problems: Vec<String> = match jsonschema::validator_for(&schema) {
        Ok(validator) => validator.iter_errors(&raw).map(|e| e.to_string()).collect(),
        Err(e) => vec![e.to_string()],
    };
    if !problems.is_empty() {
        return Err((raw, problems));
    }
    serde_json::from_value(raw.clone()).map_err(|e| (raw, vec![e.to_string()]))
}

fn settle(mut state: DraftState) -> DraftState {
    state.settled = true;
    state.problems = Vec::new();
    state.correction = String::new();
    state
}

fn step_request_content(
    goal: &str,
    outline: &[String],
    steps: &Plan,
    entry: &str,
    next_step_id: &str,
) -> String {
    let mut content = prompts::drafting_preamble(goal, outline);
    if !steps.is_empty() {
        content.push_str(&format!(
            "\n\n# Accepted steps\n{}",
            serde_json::to_string_pretty(steps).unwrap_or_default()
        ));
    }
    let request = match outline.iter().position(|item| item.trim() == entry) {
        Some(position) if !entry.is_empty() => {
            prompts::step_request(next_step_id, position + 1, entry)
        }
        _ => prompts::closing_step_request(next_step_id),
    };
    content.push_str("\n\n");
    content.push_str(&request);
    content
}

/// The next id in the planner's E-sequence over the accepted steps
/// (first step: E0).
fn next_step_id(plan: &Plan) -> String {
    let next = plan
        .iter()
        .filter_map(|step| plan::step_number(&step.id))
        .map(|n| n + 1)
        .max()
        .unwrap_or(0);
    format!("E{next}")
}

fn correction(draft: &StepDraft, problems: &[String], next_step_id: &str) -> String {
    format!(
        "Your previous response was:\n{}\n\nThe step is invalid:\n- {}\nProduce a corrected step \
         (id {next_step_id}) for the same stage; do not re-emit accepted steps.",
        serde_json::to_string(draft).unwrap_or_default(),
        problems.join("\n- ")
    )
}

fn absorb(state: &mut DraftState, draft: &StepDraft) {
    if let Some(query) = draft
        .query_to_answer
        .as_deref()
        .filter(|query| !query.trim().is_empty())
    {
        state.solver = Some(DraftSolver {
            query_to_answer: query.to_string(),
            system_prompt: draft.system_prompt.clone(),
        });
    }
}

fn assemble_output(state: &DraftState) -> PlannerOutput {
    let plan = state.steps.clone();
    let solver_data = state.solver.as_ref().map(|solver| {
        let mut solver_data = SolverData {
            query_to_answer: solver.query_to_answer.clone(),
            system_prompt: solver.system_prompt.clone(),
            data: Map::new(),
        };
        plan::default_solver_data(&plan, &mut solver_data.data);
        solver_data
    });
    PlannerOutput { plan, solver_data }
}
