//! Plan drafting: an isolated outline call (its own prompt, tool names
//! only, the `outliner` role), then one structured LLM call per step, each
//! statically validated before acceptance. The step conversation is a
//! transient scratchpad — its system prompt is built once and reused
//! byte-identically across every step call (prompt-cache invariant), and
//! only accepted work persists as Assistant turns; failed
//! attempts live in a retry tail discarded on acceptance.

use super::plan::{self, Plan, PlannerOutput, SolverData, Step};
use super::{prompts, Pipeline, PipelineError};
use graph_config::Role;
use graph_llm::types::ChatMessage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlanOutline {
    #[schemars(
        description = "The plan's stages in the order they run, each a one- or two-sentence brief on what that stage of the system is responsible for."
    )]
    pub entries: Vec<String>,
}

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

/// After every outline stage has a step, allow at most this many extra
/// steps before force-completing — the outline was covered, so the plan
/// is almost certainly done even if the planner didn't say so.
const MAX_OVERFLOW_STEPS: usize = 2;

impl Pipeline {
    /// Ask the planner for a draft: outline → one validated step per
    /// call, each step statically validated as it is drafted (so the
    /// caller never has to validate the whole plan). Nothing executes.
    ///
    /// `existing` seeds the draft with a plan's identity and prior steps —
    /// used to draft into an already-named plan, not to "revise" one. There
    /// is deliberately no feedback parameter: redrafting replaces every step,
    /// so steering the planner at a plan that already has hand-tuned steps
    /// discards them. Corrections belong in the editing commands
    /// (`plan set`, `plan step update`), which apply one intent and are
    /// validated atomically. Per-step validation problems are still fed back
    /// inside the step loop below; that is a different mechanism.
    ///
    /// See the module docs for the conversation discipline.
    pub async fn draft_plan(
        &self,
        query: &str,
        existing: Option<&PlannerOutput>,
    ) -> Result<PlannerOutput, PipelineError> {
        self.events.planning();
        let draft = existing.map(|output| serde_json::to_string_pretty(output).unwrap_or_default());
        // Built once; every call in this session reuses it byte-identically.
        let system = self.drafting_system(draft.as_deref()).await;

        let outline: PlanOutline = crate::usage::CallSite::role("outliner")
            .at("draft/outline")
            .scope(self.router.get_structured(
                Role::Outliner,
                self.outliner_system().await,
                vec![ChatMessage::User {
                    content: prompts::outline_request(query),
                }],
                "plan_outline",
            ))
            .await?;
        let entries: Vec<String> = outline
            .entries
            .into_iter()
            .map(|entry| entry.trim().to_string())
            .filter(|entry| !entry.is_empty())
            .collect();
        if entries.is_empty() {
            return Err(PipelineError::InvalidPlan("outline has no entries".into()));
        }
        self.events.draft_outline(&json!(entries));

        let mut messages: Vec<ChatMessage> = Vec::new();
        let mut brief = SolverBrief::default();
        let max_draft_steps = (2 * entries.len()).max(8);
        let mut plan: Plan = Vec::new();
        let mut complete = false;
        'stages: for index in 0..max_draft_steps {
            if index >= entries.len() + MAX_OVERFLOW_STEPS {
                // Outline fully covered and the planner still hasn't signaled
                // done; a covered plan is almost certainly complete — close it
                // rather than fail the whole draft.
                complete = true;
                break 'stages;
            }
            // Once every outline stage has a step we stop re-feeding the last
            // stage's summary and instead apply closing pressure.
            let closing = index >= entries.len();
            let next_step_id = next_step_id(&plan);
            let (summary, request) = if closing {
                (
                    "finalize the plan",
                    ChatMessage::User {
                        content: prompts::closing_step_request(&next_step_id),
                    },
                )
            } else {
                let entry = entries[index].as_str();
                let request = prompts::step_request(&next_step_id, index + 1, entry);
                let content = if index == 0 {
                    format!(
                        "{}\n\n{request}",
                        prompts::drafting_preamble(query, &entries)
                    )
                } else {
                    request
                };
                (entry, ChatMessage::User { content })
            };
            self.events.draft_step_started(index, summary);

            // Failed attempts accumulate here and are discarded on
            // acceptance — the persistent history carries only valid work.
            let mut retry_tail: Vec<ChatMessage> = Vec::new();
            let mut last_problems: Vec<String> = Vec::new();
            let mut accepted = false;
            for attempt in 1..=MAX_STEP_ATTEMPTS {
                let mut call_messages = messages.clone();
                call_messages.push(request.clone());
                call_messages.extend(retry_tail.iter().cloned());
                let step_draft: StepDraft = crate::usage::CallSite::role("planner")
                    .at(format!("draft/step.{index}"))
                    .scope(self.router.get_structured(
                        Role::Planner,
                        system.clone(),
                        call_messages,
                        "plan_step",
                    ))
                    .await?;

                let Some(step) = step_draft.step.clone() else {
                    if step_draft.plan_complete && !plan.is_empty() {
                        brief.absorb(&step_draft);
                        // Done early: the accepted steps already complete
                        // the plan. No accept event — nothing was drafted.
                        complete = true;
                        break 'stages;
                    }
                    let problems = vec!["produced no step for an incomplete plan".to_string()];
                    self.events
                        .draft_step_finished(index, &Value::Null, &problems, attempt);
                    push_correction(&mut retry_tail, &step_draft, &problems, &next_step_id);
                    last_problems = problems;
                    continue;
                };

                let mut candidate = plan.clone();
                candidate.push(step.clone());
                match self.validate_plan(&candidate) {
                    Ok(()) => {
                        self.events
                            .draft_step_finished(index, &json!(step), &[], attempt);
                        // Persist the request and the accepted draft;
                        // the retry tail is dropped with this scope.
                        messages.push(request.clone());
                        messages.push(ChatMessage::Assistant {
                            content: Some(serde_json::to_string(&step_draft).unwrap_or_default()),
                            thinking: Vec::new(),
                            tool_calls: vec![],
                        });
                        plan = candidate;
                        brief.absorb(&step_draft);
                        if step_draft.plan_complete {
                            complete = true;
                            break 'stages;
                        }
                        accepted = true;
                        break;
                    }
                    Err(problems) => {
                        self.events
                            .draft_step_finished(index, &json!(step), &problems, attempt);
                        push_correction(&mut retry_tail, &step_draft, &problems, &next_step_id);
                        last_problems = problems;
                    }
                }
            }
            if !accepted {
                return Err(PipelineError::DraftStepExhausted {
                    step_id: next_step_id,
                    attempts: MAX_STEP_ATTEMPTS,
                    problems: last_problems,
                    partial: Box::new(assemble_output(&brief, plan)),
                });
            }
        }
        if !complete {
            let step_id = next_step_id(&plan);
            return Err(PipelineError::DraftStepExhausted {
                step_id,
                attempts: 0,
                problems: vec![format!(
                    "step budget exhausted: {max_draft_steps} steps drafted \
                     without the planner marking the plan complete"
                )],
                partial: Box::new(assemble_output(&brief, plan)),
            });
        }
        Ok(assemble_output(&brief, plan))
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

#[derive(Default)]
struct SolverBrief {
    query_to_answer: Option<String>,
    system_prompt: Option<String>,
}

impl SolverBrief {
    fn absorb(&mut self, draft: &StepDraft) {
        if let Some(query) = draft
            .query_to_answer
            .as_deref()
            .filter(|query| !query.trim().is_empty())
        {
            self.query_to_answer = Some(query.to_string());
            self.system_prompt = draft.system_prompt.clone();
        }
    }
}

fn assemble_output(brief: &SolverBrief, plan: Plan) -> PlannerOutput {
    let solver_data = brief.query_to_answer.as_deref().map(|query| {
        let mut solver_data = SolverData {
            query_to_answer: query.to_string(),
            system_prompt: brief.system_prompt.clone(),
            data: Map::new(),
        };
        plan::default_solver_data(&plan, &mut solver_data.data);
        solver_data
    });
    PlannerOutput { plan, solver_data }
}
