use super::plan::{self, Plan, PlannerOutput, SolverData, Step};
use super::{prompts, Pipeline};
use graph_config::Role;
use graph_llm::types::ChatMessage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const DRAFT_STEP_TOOL: &str = "builtin__draft_step";

pub const DRAFT_PLAN: &str = "draft";

/// One step-drafting response.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StepDraft {
    /// The next step of the plan, using the requested id. Null (or
    /// omitted) with planComplete true when the accepted steps already
    /// complete the plan.
    #[serde(default)]
    pub step: Option<Step>,
    /// True when the plan is complete — after this step, or already
    /// (step null).
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
}

pub struct Draft {
    pub output: PlannerOutput,
    pub failed_step: Option<String>,
    pub problems: Vec<String>,
}

impl Draft {
    pub fn from_result(result: &Value) -> Result<Self, String> {
        let state: DraftState = serde_json::from_value(result.clone())
            .map_err(|e| format!("the draft plan returned an unexpected shape: {e}"))?;
        Ok(Self {
            failed_step: state.failed.then(|| next_step_id(&state.steps)),
            output: assemble_output(&state),
            problems: state.problems,
        })
    }
}

pub fn draft_input(goal: &str, existing: Option<&PlannerOutput>) -> Value {
    let mut input = json!({ "goal": goal });
    if let Some(existing) = existing {
        input["revising"] = json!(serde_json::to_string_pretty(existing).unwrap_or_default());
    }
    input
}

#[derive(Debug, Deserialize)]
struct DraftStepInput {
    goal: String,
    outline: Vec<String>,
    #[serde(default)]
    entry: String,
    #[serde(default)]
    state: Option<DraftState>,
    #[serde(default)]
    revising: String,
}

pub fn draft_step_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: DRAFT_STEP_TOOL.to_string(),
        description: "Drafts the next plan step advancing one outline entry, validates it against \
                      the steps already accepted, and retries with the problems. Returns the \
                      updated drafting state. An empty entry finishes the plan."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["goal", "outline", "state"],
            "properties": {
                "goal": {"type": "string", "description": "What the plan should accomplish"},
                "outline": {"type": "array", "items": {"type": "string"}, "description": "The whole outline, in order"},
                "entry": {"type": "string", "description": "The outline entry this step advances; empty to finish the plan"},
                "state": {"type": "object", "description": "The drafting state from the previous call"},
                "revising": {"type": "string", "description": "A draft plan being revised, as YAML"}
            }
        }),
        output_schema: None,
        output_example: None,
        read_only: Some(true),
    }
}

impl Pipeline {
    pub(super) async fn draft_step(&self, input: Value) -> Result<Value, String> {
        let input: DraftStepInput =
            serde_json::from_value(input).map_err(|e| format!("invalid draft_step input: {e}"))?;
        if input.outline.iter().all(|entry| entry.trim().is_empty()) {
            return Err("the outline has no entries".to_string());
        }
        let mut state = input.state.unwrap_or_default();
        if state.done || state.failed {
            return Ok(json!(state));
        }
        let index = state.steps.len();
        let next_step_id = next_step_id(&state.steps);
        let entry = input.entry.trim();
        if state.steps.is_empty() && input.outline.first().map(|first| first.trim()) == Some(entry)
        {
            self.events.draft_outline(&json!(input.outline));
        }
        let summary = if entry.is_empty() {
            "finalize the plan"
        } else {
            entry
        };
        self.events.draft_step_started(index, summary);

        let revising = (!input.revising.trim().is_empty()).then_some(input.revising.as_str());
        let system = self.drafting_system(revising).await;
        let request = ChatMessage::User {
            content: step_request_content(
                &input.goal,
                &input.outline,
                &state.steps,
                entry,
                &next_step_id,
            ),
        };

        let mut retry_tail: Vec<ChatMessage> = Vec::new();
        let mut last_problems: Vec<String> = Vec::new();
        for attempt in 1..=MAX_STEP_ATTEMPTS {
            let mut messages = vec![request.clone()];
            messages.extend(retry_tail.iter().cloned());
            let step_draft: StepDraft = crate::usage::CallSite::role("planner")
                .at(format!("draft/step.{index}"))
                .scope(self.router.get_structured(
                    Role::Planner,
                    system.clone(),
                    messages,
                    "plan_step",
                ))
                .await
                .map_err(|e| e.to_string())?;

            let Some(step) = step_draft.step.clone() else {
                if step_draft.plan_complete && !state.steps.is_empty() {
                    absorb(&mut state, &step_draft);
                    state.done = true;
                    return Ok(json!(state));
                }
                let problems = vec!["produced no step for an incomplete plan".to_string()];
                self.events
                    .draft_step_finished(index, &Value::Null, &problems, attempt);
                push_correction(&mut retry_tail, &step_draft, &problems, &next_step_id);
                last_problems = problems;
                continue;
            };

            let mut candidate = state.steps.clone();
            candidate.push(step.clone());
            match self.validate_plan(&candidate) {
                Ok(()) => {
                    self.events
                        .draft_step_finished(index, &json!(step), &[], attempt);
                    state.steps = candidate;
                    absorb(&mut state, &step_draft);
                    state.done = step_draft.plan_complete;
                    return Ok(json!(state));
                }
                Err(problems) => {
                    self.events
                        .draft_step_finished(index, &json!(step), &problems, attempt);
                    push_correction(&mut retry_tail, &step_draft, &problems, &next_step_id);
                    last_problems = problems;
                }
            }
        }
        state.failed = true;
        state.problems = last_problems;
        Ok(json!(state))
    }

    /// The system prompt for a drafting session — the same catalog/shape
    /// gathering as `planner_system` (the shape cache is read fresh here,
    /// at drafting time), rendered through the drafting prompt.
    async fn drafting_system(&self, draft: Option<&str>) -> String {
        let (tools_text, step_schema) = self.planner_catalog().await;

        prompts::drafting_prompt(&prompts::DraftingPromptArgs {
            current_date: &self.current_date,
            tools: &tools_text,
            user_context: &self.user_context,
            step_schema: &step_schema,
            draft,
        })
    }
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

/// Append one failed attempt and its correction request to the retry tail.
fn push_correction(
    retry_tail: &mut Vec<ChatMessage>,
    step_draft: &StepDraft,
    problems: &[String],
    next_step_id: &str,
) {
    retry_tail.push(ChatMessage::Assistant {
        content: Some(serde_json::to_string(step_draft).unwrap_or_default()),
        thinking: Vec::new(),
        tool_calls: vec![],
    });
    retry_tail.push(ChatMessage::User {
        content: format!(
            "The step is invalid:\n- {}\nProduce a corrected step (id {next_step_id}) \
             for the same stage; do not re-emit accepted steps.",
            problems.join("\n- ")
        ),
    });
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
