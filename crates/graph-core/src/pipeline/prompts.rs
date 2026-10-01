//! Planner and solver prompts — ported from the original
//! `plannerPrompt.ts`/`solverPrompt.ts`, trimmed to this runtime's actual
//! capabilities (no expectations, no datetime tool, no artifacts) and
//! updated for the strict template dialect.

use crate::store::ToolShape;
use crate::tools::ToolDef;
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub const TEMPLATING_RULES: &str = include_str!("prompts/templating_rules.md").trim_ascii_end();

/// Control-step usage rules, shared verbatim between the draft_plan
/// planner prompt and the workbench chat agent's system prompt so the two
/// cannot drift.
pub const CONTROL_STEP_RULES: &str = include_str!("prompts/control_step_rules.md").trim_ascii_end();

/// Planning rules shared verbatim by the planner and drafting
/// prompts, which differ only in how they are called.
const PLANNING_RULES: &str = include_str!("prompts/planning_rules.md").trim_ascii_end();

pub fn outliner_prompt(tools: &str) -> String {
    format!(include_str!("prompts/outliner.md"), tools = tools)
        .trim_ascii_end()
        .to_string()
}

const BUILTIN_SUMMARIES: &[(&str, &str)] = &[
    (
        "builtin__infer",
        "one LLM call over a self-contained instruction, returning text or JSON validated against a schema.",
    ),
    (
        "builtin__reshape",
        "deterministically rebuilds data into a new JSON shape (rename, select, nest, flatten, interpolate) without an LLM.",
    ),
];

fn described(name: &str, description: Option<&str>) -> String {
    match description {
        Some(description) => format!("- {name}: {description}"),
        None => format!("- {name}"),
    }
}

pub fn outliner_catalog(names: &[String], servers: &[crate::tools::ToolServer]) -> String {
    let mut mcp: Vec<String> = Vec::new();
    let mut packs: Vec<String> = Vec::new();
    let mut builtins: Vec<String> = Vec::new();
    let mut user: Vec<String> = Vec::new();
    let mut plans: Vec<String> = Vec::new();
    let mut seen_packs: HashSet<&str> = HashSet::new();
    let mut seen_servers: HashSet<&str> = HashSet::new();
    for name in names {
        let summary = BUILTIN_SUMMARIES
            .iter()
            .find(|(tool, _)| tool == name)
            .map(|(_, summary)| *summary);
        match name.split_once("__") {
            Some(("builtin", tool)) => match crate::user_tools::pack_of(tool) {
                Some(pack)
                    if summary.is_none() && !crate::user_tools::DEFAULT_PACKS.contains(&pack) =>
                {
                    if seen_packs.insert(pack) {
                        packs.push(described(pack, crate::user_tools::pack_summary(pack)));
                    }
                }
                _ => builtins.push(described(name, summary)),
            },
            Some(("user", _)) => user.push(format!("- {name}")),
            Some(("plan", _)) => plans.push(format!("- {name}")),
            Some((server, _)) => {
                if seen_servers.insert(server) {
                    let description = servers
                        .iter()
                        .find(|known| known.name == server)
                        .and_then(|known| known.description.as_deref());
                    mcp.push(described(server, description));
                }
            }
            None => builtins.push(described(name, summary)),
        }
    }
    let sections: Vec<String> = [
        ("MCP Servers", mcp),
        ("Tool packs", packs),
        ("Builtin tools", builtins),
        ("User tools", user),
        ("Plans", plans),
    ]
    .into_iter()
    .filter(|(_, lines)| !lines.is_empty())
    .map(|(title, lines)| format!("## {title}\n{}", lines.join("\n")))
    .collect();
    if sections.is_empty() {
        return "## Tools\nNo tools are configured.".to_string();
    }
    sections.join("\n\n")
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
             every step, starting from the first.\n\
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

/// The outliner's only turn: the task, nothing else.
pub fn outline_request(query: &str) -> String {
    format!("# Task\n{query}")
}

pub fn drafting_preamble(query: &str, entries: &[String]) -> String {
    let outline: Vec<String> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| format!("{}. {entry}", index + 1))
        .collect();
    format!("# Task\n{query}\n\n# Outline\n{}", outline.join("\n"))
}

/// One step request: names the id the step must use and the outline entry
/// it (advisorily) advances.
pub fn step_request(next_step_id: &str, entry_number: usize, entry: &str) -> String {
    format!(
        "Produce step {next_step_id}, advancing outline entry {entry_number}:\n\
         {entry}\n\n\
         Emit exactly one step — or step: null with planComplete: true if \
         the accepted steps already complete the plan."
    )
}

/// A closing step request used once every outline stage already has a
/// step: push the planner to finish rather than re-draft the last stage.
pub fn closing_step_request(next_step_id: &str) -> String {
    format!(
        "Every outline entry has now been advanced. If the plan is complete, return \
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
            prompt.contains("is ONE step"),
            "a control step must be exactly one step"
        );
        assert!(
            prompt.contains("on your FIRST step response"),
            "the solver brief rides on the first step draft"
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
    fn request_helpers_name_ids_and_entries() {
        assert_eq!(outline_request("do the thing"), "# Task\ndo the thing");
        let request = step_request("E2", 3, "fetch the issues");
        assert!(request.contains("step E2"));
        assert!(request.contains("outline entry 3:\nfetch the issues"));
        assert!(request.contains("planComplete: true"));
    }

    #[test]
    fn drafting_preamble_numbers_every_outline_entry() {
        let preamble = drafting_preamble(
            "report on x",
            &["gather x".to_string(), "summarize it".to_string()],
        );
        assert_eq!(
            preamble,
            "# Task\nreport on x\n\n# Outline\n1. gather x\n2. summarize it"
        );
    }

    #[test]
    fn the_outliner_prompt_carries_names_but_no_schemas() {
        let tools = outliner_catalog(&["builtin__git_log".to_string()], &[]);
        let prompt = outliner_prompt(&tools);
        assert!(prompt.contains("## Tool packs\n- github: "), "{prompt}");
        assert!(prompt.contains("principal engineer"));
        for step in ["exit", "route", "filter", "map", "reduce", "agent", "ask"] {
            assert!(
                prompt.contains(&format!("- `{step}`: ")),
                "{step} is described"
            );
            assert!(super::super::is_control_step(step));
        }
        assert!(!prompt.contains("inputSchema"));
        assert!(!prompt.contains("templating_rules"));
    }

    #[test]
    fn the_outliner_catalog_sections_servers_packs_builtins_user_tools_and_plans() {
        let names: Vec<String> = [
            "builtin__git_diff",
            "linear__list_issues",
            "builtin__infer",
            "user__summarize",
            "linear__get_issue",
            "github__search_code",
            "plan__sprint_analysis",
            "builtin__reshape",
            "builtin__git_log",
            "builtin__slack_post_message",
        ]
        .map(String::from)
        .to_vec();
        let servers = [
            crate::tools::ToolServer {
                name: "linear".into(),
                description: Some("Issue tracking".into()),
            },
            crate::tools::ToolServer {
                name: "github".into(),
                description: None,
            },
        ];
        let catalog = outliner_catalog(&names, &servers);
        let sections: Vec<&str> = catalog.split("\n\n").collect();
        assert_eq!(
            sections[0],
            "## MCP Servers\n- linear: Issue tracking\n- github"
        );
        assert!(
            sections[1].starts_with("## Tool packs\n- github: local git history"),
            "{catalog}"
        );
        assert!(
            sections[1].contains("\n- slack: posts messages"),
            "{catalog}"
        );
        assert_eq!(sections[1].lines().count(), 3, "one line per pack");
        assert!(
            sections[2].starts_with("## Builtin tools\n- builtin__infer: one LLM call"),
            "{catalog}"
        );
        assert!(
            sections[2].contains("\n- builtin__reshape: deterministically"),
            "{catalog}"
        );
        assert_eq!(sections[3], "## User tools\n- user__summarize");
        assert_eq!(sections[4], "## Plans\n- plan__sprint_analysis");
        assert_eq!(sections.len(), 5);
        assert!(
            !catalog.contains("list_issues"),
            "MCP tool lists are dropped"
        );
        assert!(!catalog.contains("git_diff"), "pack tool lists are dropped");
        assert_eq!(
            outliner_catalog(&[], &[]),
            "## Tools\nNo tools are configured."
        );
    }

    #[test]
    fn closing_step_request_pushes_the_planner_to_finish() {
        let request = closing_step_request("E5");
        assert!(request.contains("Every outline entry has now been advanced"));
        assert!(request.contains("planComplete: true"));
        assert!(request.contains("E5"));
    }
}
