//! Planner and solver prompts — ported from the original
//! `plannerPrompt.ts`/`solverPrompt.ts`, trimmed to this runtime's actual
//! capabilities (no expectations, no datetime tool, no artifacts) and
//! updated for the strict template dialect.

use crate::store::ToolShape;
use crate::tools::ToolDef;
use serde_json::json;
use std::collections::HashMap;

pub const TEMPLATING_RULES: &str = include_str!("prompts/templating_rules.md").trim_ascii_end();

/// Control-step usage rules, shared verbatim between the draft_plan
/// planner prompt and the workbench chat agent's system prompt so the two
/// cannot drift.
pub const CONTROL_STEP_RULES: &str = include_str!("prompts/control_step_rules.md").trim_ascii_end();

/// Planning rules shared verbatim by the planner and drafting
/// prompts, which differ only in how they are called.
const PLANNING_RULES: &str = include_str!("prompts/planning_rules.md").trim_ascii_end();

pub struct PlannerPromptArgs<'a> {
    pub current_date: &'a str,
    pub last_error: Option<&'a str>,
    pub next_step_id: &'a str,
    pub tools: &'a str,
    pub user_context: &'a str,
    pub existing_plan: &'a str,
    pub step_schema: &'a str,
}

pub fn planner_prompt(args: &PlannerPromptArgs) -> String {
    let last_error = args.last_error.unwrap_or("none");
    format!(
        include_str!("prompts/planner.md"),
        current_date = args.current_date,
        last_error = last_error,
        next_step_id = args.next_step_id,
        tools = args.tools,
        templating_rules = TEMPLATING_RULES,
        user_context = args.user_context,
        existing_plan = args.existing_plan,
        step_schema = args.step_schema,
        planning_rules = PLANNING_RULES,
        control_step_rules = CONTROL_STEP_RULES,
    )
}

pub struct DraftingPromptArgs<'a> {
    pub current_date: &'a str,
    pub tools: &'a str,
    pub user_context: &'a str,
    pub step_schema: &'a str,
    /// A draft plan under revision (workbench). Nothing in it has
    /// executed: every step is mutable, and the revision regenerates the
    /// plan in full — outline first, then steps.
    pub draft: Option<&'a str>,
}

/// The system prompt for plan drafting. Built once per drafting session
/// and reused byte-identically for the outline call and every step call,
/// so the provider's prompt-cache prefix stays stable.
pub fn drafting_prompt(args: &DraftingPromptArgs) -> String {
    let draft_section = match args.draft {
        Some(draft) => format!(
            "### Draft Under Revision\nThe following draft plan has NOT been executed. \
             Revise it according to the user's request — you may modify, reorder, \
             remove, or replace any step. Output the COMPLETE revised plan, not a diff: \
             a fresh outline, then every step.\n\
             <draft_plan>\n{draft}\n</draft_plan>\n\n"
        ),
        None => String::new(),
    };
    format!(
        include_str!("prompts/drafting.md"),
        current_date = args.current_date,
        tools = args.tools,
        templating_rules = TEMPLATING_RULES,
        user_context = args.user_context,
        draft_section = draft_section,
        step_schema = args.step_schema,
        planning_rules = PLANNING_RULES,
        control_step_rules = CONTROL_STEP_RULES,
    )
}

/// The first user turn of a drafting session: ask for the outline.
pub fn outline_request(query: &str) -> String {
    format!("Produce the plan outline for this task.\n\n# Task\n{query}")
}

/// One step request: names the id the step must use and the outline stage
/// it (advisorily) corresponds to.
pub fn step_request(next_step_id: &str, stage_number: usize, summary: &str) -> String {
    format!(
        "Produce step {next_step_id} (stage {stage_number}: {summary}). \
         Emit exactly one step — or step: null with planComplete: true if \
         the accepted steps already complete the plan."
    )
}

/// A closing step request used once every outline stage already has a
/// step: push the planner to finish rather than re-draft the last stage.
pub fn closing_step_request(next_step_id: &str) -> String {
    format!(
        "Every outline stage now has a step. If the plan is complete, return \
         step: null with planComplete: true. Only if one concrete additional \
         step is genuinely required to finish the plan, emit exactly that step \
         as {next_step_id} and set planComplete: true on it."
    )
}

/// Describe tools for the planner: name, description, input schema, and the
/// best available output shape (declared schema > override > observed).
pub fn describe_tools(tools: &[ToolDef], shapes: &HashMap<String, ToolShape>) -> String {
    let mut out = String::new();
    for tool in tools {
        let mut entry = json!({
            "name": tool.name,
            "description": tool.description,
            "inputSchema": tool.input_schema,
        });
        if let Some(schema) = &tool.output_schema {
            entry["outputSchema"] = schema.clone();
        }
        if let Some(example) = &tool.output_example {
            entry["outputExample"] = example.clone();
        }
        if entry.get("outputSchema").is_none() && entry.get("outputExample").is_none() {
            if let Some(shape) = shapes.get(&tool.name) {
                entry["observedOutputShape"] = shape.schema.clone();
                entry["observedOutputExample"] = shape.example.clone();
            }
        }
        out.push_str(&serde_json::to_string(&entry).unwrap_or_default());
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("(no tools available)");
    }
    out
}

pub const SOLVER_SYSTEM_PROMPT: &str = include_str!("prompts/solver_system.md").trim_ascii_end();

pub const ERROR_SUMMARY_PROMPT: &str = include_str!("prompts/error_summary.md").trim_ascii_end();

#[cfg(test)]
mod tests {
    use super::*;

    /// The control-step rules carry two deliberate steering behaviors;
    /// keep them from being edited away silently.
    #[test]
    fn control_step_rules_carry_steering_guidance() {
        assert!(
            CONTROL_STEP_RULES.contains("check or assertion"),
            "check-shaped plans must be steered toward explicit gated exits"
        );
        assert!(
            CONTROL_STEP_RULES.contains("per-item inference call"),
            "list inference must be steered toward map with per-item calls"
        );
    }

    #[test]
    fn planner_prompt_includes_control_step_rules() {
        let prompt = planner_prompt(&PlannerPromptArgs {
            current_date: "2026-01-01",
            last_error: None,
            next_step_id: "E0",
            tools: "(no tools available)",
            user_context: "(none)",
            existing_plan: "(none)",
            step_schema: "{}",
        });
        assert!(prompt.contains(CONTROL_STEP_RULES));
        assert!(prompt.contains(PLANNING_RULES));
    }

    fn drafting_prompt_for(draft: Option<&str>) -> String {
        drafting_prompt(&DraftingPromptArgs {
            current_date: "2026-01-01",
            tools: "(no tools available)",
            user_context: "(none)",
            step_schema: "{}",
            draft,
        })
    }

    #[test]
    fn drafting_prompt_carries_the_shared_sections() {
        let prompt = drafting_prompt_for(None);
        assert!(prompt.contains(CONTROL_STEP_RULES));
        assert!(prompt.contains(PLANNING_RULES));
        assert!(prompt.contains(TEMPLATING_RULES));
        assert!(!prompt.contains("Draft Under Revision"));
    }

    #[test]
    fn drafting_prompt_teaches_the_drafting_protocol() {
        let prompt = drafting_prompt_for(None);
        assert!(
            prompt.contains("is ONE stage"),
            "a control step must be exactly one outline stage"
        );
        assert!(
            prompt.contains("`step: null` with `planComplete: true`"),
            "the done-early convention must be taught"
        );
        assert!(
            prompt.contains("Never re-emit accepted steps"),
            "the correction protocol must be taught"
        );
    }

    #[test]
    fn drafting_prompt_revision_slot_carries_the_draft() {
        let prompt = drafting_prompt_for(Some("{\"plan\": []}"));
        assert!(prompt.contains("Draft Under Revision"));
        assert!(prompt.contains("{\"plan\": []}"));
    }

    #[test]
    fn request_helpers_name_ids_and_stages() {
        assert!(outline_request("do the thing").contains("do the thing"));
        let request = step_request("E2", 3, "fetch the issues");
        assert!(request.contains("step E2"));
        assert!(request.contains("stage 3: fetch the issues"));
        assert!(request.contains("planComplete: true"));
    }

    #[test]
    fn closing_step_request_pushes_the_planner_to_finish() {
        let request = closing_step_request("E5");
        assert!(request.contains("Every outline stage now has a step"));
        assert!(request.contains("planComplete: true"));
        assert!(request.contains("E5"));
    }
}
