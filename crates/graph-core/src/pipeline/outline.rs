//! Incremental plan drafting: an outline call, then one structured LLM
//! call per step, each statically validated before acceptance. The
//! conversation is a transient scratchpad — the system prompt is built
//! once and reused byte-identically across every call (prompt-cache
//! invariant), and only accepted work persists as Assistant turns; failed
//! attempts live in a retry tail discarded on acceptance.

use super::plan::{self, Plan, PlannerOutput, SolverData, Step};
use super::{prompts, Pipeline, PipelineError};
use graph_config::Role;
use graph_llm::types::ChatMessage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The rough shape of the plan, produced before any real step. Field names
/// are camelCase because they are prompt surface.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanOutline {
    /// The plan's stages, in order: 2–8 one-sentence summaries. A control
    /// step (decide, map, reduce) is one stage — its body nests inside
    /// that step.
    pub items: Vec<OutlineItem>,
    /// The question the solver must answer; always includes the user's
    /// original task.
    #[serde(default)]
    pub query_to_answer: String,
    /// Extra system-prompt guidance for the solver (optional).
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// One outline stage.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutlineItem {
    /// One sentence: what this stage accomplishes.
    pub summary: String,
    /// The exact catalog tool name expected to accomplish it, when known.
    #[serde(default)]
    pub expected_tool: Option<String>,
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
}

/// Per-step retry budget: attempts at producing a step that passes static
/// validation before drafting gives up with the valid partial. Exported
/// (via the pipeline module) so the workbench can label attempts against
/// the same ceiling without hardcoding it.
pub const MAX_STEP_ATTEMPTS: u32 = 3;

impl Pipeline {
    /// The incremental strategy: outline → one validated step per call.
    /// See the module docs for the conversation discipline.
    pub(super) async fn draft_plan_incremental(
        &self,
        query: &str,
        existing: Option<&PlannerOutput>,
        last_error: Option<&str>,
    ) -> Result<PlannerOutput, PipelineError> {
        self.events.planning();
        let draft = existing.map(|output| serde_json::to_string_pretty(output).unwrap_or_default());
        // Built once; every call in this session reuses it byte-identically.
        let system = self
            .incremental_planner_system(last_error, draft.as_deref())
            .await;

        let mut messages = vec![ChatMessage::User {
            content: prompts::outline_request(query),
        }];
        let outline: PlanOutline = self
            .router
            .get_structured(
                Role::Planner,
                system.clone(),
                messages.clone(),
                "plan_outline",
            )
            .await?;
        if outline.items.is_empty() {
            return Err(PipelineError::InvalidPlan("outline has no items".into()));
        }
        self.events.draft_outline(&json!(outline.items));
        messages.push(ChatMessage::Assistant {
            content: Some(serde_json::to_string(&outline).unwrap_or_default()),
            tool_calls: vec![],
        });

        // The outline is advisory, so the step budget is not its length:
        // stages may split, but runaway drafting must still terminate.
        let max_draft_steps = (2 * outline.items.len()).max(8);
        let mut plan: Plan = Vec::new();
        let mut complete = false;
        'stages: for index in 0..max_draft_steps {
            // Advisory stage mapping: past the outline's end the last
            // stage's summary carries (steps may outnumber stages).
            let summary = outline
                .items
                .get(index.min(outline.items.len() - 1))
                .map(|item| item.summary.as_str())
                .unwrap_or("additional step");
            let next_step_id = next_step_id(&plan);
            self.events.draft_step_started(index, summary);
            let request = ChatMessage::User {
                content: prompts::step_request(&next_step_id, index + 1, summary),
            };

            // Failed attempts accumulate here and are discarded on
            // acceptance — the persistent history carries only valid work.
            let mut retry_tail: Vec<ChatMessage> = Vec::new();
            let mut last_problems: Vec<String> = Vec::new();
            let mut accepted = false;
            for attempt in 1..=MAX_STEP_ATTEMPTS {
                let mut call_messages = messages.clone();
                call_messages.push(request.clone());
                call_messages.extend(retry_tail.iter().cloned());
                let step_draft: StepDraft = self
                    .router
                    .get_structured(Role::Planner, system.clone(), call_messages, "plan_step")
                    .await?;

                let Some(step) = step_draft.step.clone() else {
                    if step_draft.plan_complete && !plan.is_empty() {
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
                            tool_calls: vec![],
                        });
                        plan = candidate;
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
                    partial: Box::new(assemble_output(&outline, plan)),
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
                partial: Box::new(assemble_output(&outline, plan)),
            });
        }
        Ok(assemble_output(&outline, plan))
    }

    /// The system prompt for an incremental drafting session — the same
    /// catalog/shape gathering as `planner_system` (the shape cache is
    /// read fresh here, at drafting time), rendered through the
    /// incremental prompt.
    async fn incremental_planner_system(
        &self,
        last_error: Option<&str>,
        draft: Option<&str>,
    ) -> String {
        let (tools_text, step_schema) = self.planner_catalog().await;

        prompts::incremental_planner_prompt(&prompts::IncrementalPlannerPromptArgs {
            current_date: &self.current_date,
            last_error,
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

/// Solver data comes from the outline (no extra inference); `data`
/// defaults to every step result, exactly like the one-shot path.
fn assemble_output(outline: &PlanOutline, plan: Plan) -> PlannerOutput {
    let mut output = PlannerOutput {
        plan,
        solver_data: SolverData {
            query_to_answer: outline.query_to_answer.clone(),
            system_prompt: outline.system_prompt.clone(),
            data: Map::new(),
        },
    };
    plan::default_solver_data(&output.plan, &mut output.solver_data.data);
    output
}
