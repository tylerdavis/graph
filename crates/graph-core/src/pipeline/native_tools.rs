use super::authoring;
use super::compose::{
    compose_tool_defs, question_form, CHECK_PLAN_TOOL, COMPOSE_CONTEXT_TOOL, FIND_TOOLS_TOOL,
    QUESTION_FORM_TOOL,
};
use super::doc::{parse_plan_source, PlanDoc};
use super::drafter::{drafter_tool_defs, SAVE_AGENT_TOOL, SAVE_TOOL_TOOL, TRY_TOOL_TOOL};
use super::drafting::{
    accept_step_tool_def, draft_context_tool_def, ACCEPT_STEP_TOOL, DRAFT_CONTEXT_TOOL,
};
use super::search::{
    search_tool_defs, CATALOG_TOOLS_TOOL, DESCRIBE_TOOLS_TOOL, SCORE_CANDIDATES_TOOL,
};
use super::{prompts, Draft, Pipeline, Step};
use crate::tools::{ToolDef, ToolOutcome};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const CATALOG_OUTLINE_TOOL: &str = "builtin__catalog_outline";

pub const VALIDATE_PLAN_TOOL: &str = "builtin__validate_plan";

pub const PLAN_FROM_DRAFT_TOOL: &str = "builtin__plan_from_draft";

pub const APPLY_EDITS_TOOL: &str = "builtin__apply_edits";

pub const NATIVE_TOOLS: [&str; 16] = [
    CATALOG_OUTLINE_TOOL,
    DRAFT_CONTEXT_TOOL,
    ACCEPT_STEP_TOOL,
    VALIDATE_PLAN_TOOL,
    PLAN_FROM_DRAFT_TOOL,
    APPLY_EDITS_TOOL,
    CATALOG_TOOLS_TOOL,
    SCORE_CANDIDATES_TOOL,
    DESCRIBE_TOOLS_TOOL,
    COMPOSE_CONTEXT_TOOL,
    CHECK_PLAN_TOOL,
    QUESTION_FORM_TOOL,
    FIND_TOOLS_TOOL,
    TRY_TOOL_TOOL,
    SAVE_TOOL_TOOL,
    SAVE_AGENT_TOOL,
];

const CONTROL_TOOLS: &[&str] = &["exit", "route", "filter", "map", "reduce", "agent", "ask"];

pub fn is_native_tool(name: &str) -> bool {
    NATIVE_TOOLS.contains(&name)
}

pub fn native_tool_defs() -> Vec<ToolDef> {
    let mut defs = vec![
        ToolDef {
            name: CATALOG_OUTLINE_TOOL.to_string(),
            description: "Summarizes what a plan can use, in sections: MCP servers and tool \
                          packs by name and description, then built-in capabilities, agents, \
                          project tools and saved plans as one-line descriptions. Never tool \
                          names or schemas."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: Some(json!({
                "type": "object",
                "required": ["text"],
                "properties": {"text": {"type": "string"}}
            })),
            output_example: None,
            read_only: Some(true),
        },
        draft_context_tool_def(),
        accept_step_tool_def(),
        ToolDef {
            name: VALIDATE_PLAN_TOOL.to_string(),
            description: "Validates a plan: its own structure and templates, and every tool it \
                          calls against the catalog. Returns every problem found, and the tools \
                          it calls that aren't marked read-only (side_effects). Pass the whole \
                          plan as `plan`, or just its `steps`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {"type": "object", "description": "A plan document"},
                    "steps": {"type": "array", "items": {"type": "object"}, "description": "A plan's steps, when there is no whole plan"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["valid", "problems", "side_effects"],
                "properties": {
                    "valid": {"type": "boolean"},
                    "problems": {"type": "array", "items": {"type": "string"}},
                    "side_effects": {"type": "array", "items": {"type": "string"}}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: PLAN_FROM_DRAFT_TOOL.to_string(),
            description: "Turns what plan__draft_expand drafted into a plan document named \
                          from the goal. When a step failed to draft, the valid steps before it \
                          are kept and the problems are returned."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["goal", "draft"],
                "properties": {
                    "goal": {"type": "string", "description": "What the plan should accomplish"},
                    "draft": {"type": "object", "description": "The result plan__draft_expand returned"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["plan", "problems", "failed_step"],
                "properties": {
                    "plan": {"type": "object"},
                    "problems": {"type": "array", "items": {"type": "string"}},
                    "failed_step": {"type": ["string", "null"], "description": "The step drafting stopped at, when it failed"}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: APPLY_EDITS_TOOL.to_string(),
            description: "Applies edits to a plan document, one at a time and in order. Each \
                          edit is an object with `op` (update_metadata, add_step, update_step, \
                          delete_step) and that operation's fields, the same as the workbench \
                          editing tools. An edit that would introduce a new validation problem \
                          is rejected and the plan is left as it was before it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan", "edits"],
                "properties": {
                    "plan": {"type": "object", "description": "The plan document to edit"},
                    "edits": {"type": "array", "items": {"type": "object"}, "description": "The edits, in order"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["plan", "applied", "rejected"],
                "properties": {
                    "plan": {"type": "object"},
                    "applied": {"type": "array", "items": {"type": "object"}},
                    "rejected": {"type": "array", "items": {"type": "object"}}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
    ];
    defs.extend(search_tool_defs());
    defs.extend(compose_tool_defs());
    defs.extend(drafter_tool_defs());
    defs
}

impl Pipeline {
    pub(super) async fn catalog_outline(&self) -> String {
        let mut tools: Vec<(String, String)> = self
            .registry
            .tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|tool| (tool.name, tool.description))
            .collect();
        tools.extend(
            self.callable_plan_defs()
                .into_iter()
                .map(|tool| (tool.name, tool.description)),
        );
        let servers = self.registry.servers().await;
        let agents: Vec<(String, String)> = self
            .agents
            .iter()
            .filter(|doc| doc.name != crate::agent::doc::CHAT_AGENT)
            .map(|doc| (doc.name.clone(), doc.description.clone()))
            .collect();
        prompts::outliner_catalog(&tools, &servers, &agents)
    }

    pub(super) async fn call_native(&self, name: &str, input: Value) -> ToolOutcome {
        let result = match name {
            CATALOG_OUTLINE_TOOL => Ok(json!({ "text": self.catalog_outline().await })),
            DRAFT_CONTEXT_TOOL => self.draft_context(input).await,
            ACCEPT_STEP_TOOL => self.accept_step(input),
            VALIDATE_PLAN_TOOL => self.validate_plan_input(input).await,
            PLAN_FROM_DRAFT_TOOL => plan_from_draft(input),
            APPLY_EDITS_TOOL => apply_edits(input),
            CATALOG_TOOLS_TOOL => self.catalog_tools().await,
            SCORE_CANDIDATES_TOOL => self.score_candidates(input).await,
            DESCRIBE_TOOLS_TOOL => self.describe_tools(input).await,
            COMPOSE_CONTEXT_TOOL => self.compose_context(input).await,
            CHECK_PLAN_TOOL => self.check_plan(input).await,
            QUESTION_FORM_TOOL => question_form(input),
            FIND_TOOLS_TOOL => self.find_tools(input).await,
            TRY_TOOL_TOOL => self.try_tool(input).await,
            SAVE_TOOL_TOOL => self.save_tool(input).await,
            SAVE_AGENT_TOOL => self.save_agent(input).await,
            _ => Err(format!("unknown native tool '{name}'")),
        };
        match result {
            Ok(result) => ToolOutcome {
                result,
                is_error: false,
            },
            Err(error) => tool_error(error),
        }
    }

    async fn validate_plan_input(&self, input: Value) -> Result<Value, String> {
        let (problems, steps) = match input.get("plan") {
            Some(plan) => {
                let doc = plan_doc(plan)?;
                let catalog = self.live_catalog();
                let problems = authoring::plan_problems(&doc, &self.plans, catalog.as_ref());
                (problems, json!(doc.steps))
            }
            None => {
                let steps: Vec<Step> = serde_json::from_value(input["steps"].clone())
                    .map_err(|e| format!("invalid steps: {e}"))?;
                let problems = self.validate_plan(&steps).err().unwrap_or_default();
                (problems, json!(steps))
            }
        };
        let mut defs = self.registry.tools().await.unwrap_or_default();
        defs.extend(self.callable_plan_defs());
        let mut side_effects: Vec<String> = called_tools(&steps)
            .into_iter()
            .filter(|tool| !CONTROL_TOOLS.contains(&tool.as_str()))
            .filter(|tool| {
                defs.iter()
                    .find(|def| &def.name == tool)
                    .and_then(|def| def.read_only)
                    != Some(true)
            })
            .collect();
        side_effects.sort();
        let valid = !problems.iter().any(|problem| !problem.starts_with("note:"));
        Ok(json!({ "valid": valid, "problems": problems, "side_effects": side_effects }))
    }
}

pub fn plan_doc(value: &Value) -> Result<PlanDoc, String> {
    let yaml = serde_yaml::to_string(value).map_err(|e| format!("invalid plan: {e}"))?;
    parse_plan_source(&yaml, "plan").map_err(|e| e.to_string())
}

fn plan_json(doc: &PlanDoc) -> Result<Value, String> {
    authoring::to_json(doc).map_err(|e| e.to_string())
}

pub(super) fn called_tools(value: &Value) -> BTreeSet<String> {
    let mut tools = BTreeSet::new();
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                for key in ["tool_name", "toolName"] {
                    if let Some(Value::String(name)) = map.get(key) {
                        tools.insert(name.clone());
                    }
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    tools
}

fn plan_from_draft(input: Value) -> Result<Value, String> {
    let goal = input["goal"]
        .as_str()
        .ok_or("plan_from_draft requires a 'goal' string")?;
    let draft = Draft::from_expanded(&input["draft"])?;
    let mut problems = draft.problems;
    if let Some(step) = &draft.failed_step {
        problems.insert(
            0,
            format!("drafting stopped at step {step}; the steps before it are kept"),
        );
    }
    let doc = authoring::merge_planner_output(None, goal, draft.output.clone());
    Ok(json!({
        "plan": plan_json(&doc)?,
        "problems": problems,
        "failed_step": draft.failed_step,
    }))
}

fn apply_edits(input: Value) -> Result<Value, String> {
    let mut doc = plan_doc(&input["plan"])?;
    let edits = input["edits"]
        .as_array()
        .ok_or("apply_edits requires an 'edits' list")?;
    let mut applied = Vec::new();
    let mut rejected = Vec::new();
    for edit in edits {
        let op = edit["op"].as_str().unwrap_or_default();
        let mutate: fn(&mut PlanDoc, &Value) -> Result<Value, Value> = match op {
            "update_metadata" => authoring::patch_metadata,
            "add_step" => authoring::patch_add_step,
            "update_step" => authoring::patch_update_step,
            "delete_step" => authoring::patch_delete_step,
            _ => {
                rejected.push(json!({
                    "edit": edit,
                    "error": format!("unknown op '{op}' (use update_metadata, add_step, update_step or delete_step)"),
                }));
                continue;
            }
        };
        match authoring::apply_edit(&doc, |doc| mutate(doc, edit)) {
            Ok(accepted) => {
                applied.push(json!({ "op": op, "summary": accepted.summary }));
                doc = accepted.doc;
            }
            Err(rejection) => rejected.push(json!({ "edit": edit, "rejection": rejection.body })),
        }
    }
    Ok(json!({ "plan": plan_json(&doc)?, "applied": applied, "rejected": rejected }))
}

pub(super) fn tool_error(message: String) -> ToolOutcome {
    ToolOutcome {
        result: json!({ "error": message }),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Value {
        json!({
            "identifier": "demo",
            "name": "Demo",
            "description": "demo",
            "steps": [
                {"id": "E0", "tool_name": "t__search", "input": {"query": "x"}},
                {"id": "E1", "tool_name": "t__report", "input": {"rows": "{{E0.values}}"}},
            ],
        })
    }

    #[test]
    fn edits_apply_in_order_and_a_breaking_one_is_rejected() {
        let result = apply_edits(json!({
            "plan": plan(),
            "edits": [
                {"op": "update_step", "id": "E1", "reasoning": "the report"},
                {"op": "delete_step", "id": "E0"},
                {"op": "rename_everything"},
            ],
        }))
        .unwrap();
        assert_eq!(result["applied"].as_array().unwrap().len(), 1);
        assert_eq!(result["rejected"].as_array().unwrap().len(), 2);
        let steps = result["plan"]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2, "deleting E0 would orphan E1's reference");
        assert_eq!(steps[1]["reasoning"], json!("the report"));
    }

    #[test]
    fn a_failed_draft_keeps_its_valid_steps_and_says_where_it_stopped() {
        let result = plan_from_draft(json!({
            "goal": "report on x",
            "draft": {
                "steps": [{"id": "E0", "toolName": "t__search", "input": {"query": "x"}}],
                "solver": null,
                "done": false,
                "failed": true,
                "problems": ["E9 is not a step"],
            },
        }))
        .unwrap();
        assert_eq!(result["plan"]["steps"].as_array().unwrap().len(), 1);
        let problems = result["problems"].as_array().unwrap();
        assert!(problems[0].as_str().unwrap().contains("stopped at step E1"));
        assert_eq!(problems[1], json!("E9 is not a step"));
    }
}
