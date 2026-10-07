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
pub(super) const COMPOSING_RULES: &str =
    include_str!("prompts/composing_rules.md").trim_ascii_end();

pub(super) const PLANNING_RULES: &str = include_str!("prompts/planning_rules.md").trim_ascii_end();

pub(super) fn summary_line(description: &str) -> String {
    capability(description).trim_start_matches("- ").to_string()
}

fn capability(description: &str) -> String {
    let text = description.trim();
    let text = match text.split_once(" — ") {
        Some((head, rest)) if head.len() <= 60 => rest.trim(),
        _ => text,
    };
    let first_line = text.lines().next().unwrap_or_default();
    let sentence = match first_line.find(". ") {
        Some(end) => &first_line[..=end],
        None => first_line,
    };
    let mut line: String = sentence.chars().take(200).collect();
    if sentence.chars().count() > 200 {
        line.push('…');
    }
    format!("- {}", line.trim())
}

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
        assert!(
            CONTROL_STEP_RULES.contains("Use it only when a decision model is configured"),
            "decide gates must be steered to configs that have a decision model"
        );
        assert!(
            CONTROL_STEP_RULES.contains("Keep the data in `state`"),
            "decide gates must keep the data out of the question"
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
}
