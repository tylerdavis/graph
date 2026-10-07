use super::authoring;
use super::compose::{
    compose_tool_defs, question_form, CHECK_PLAN_TOOL, COMPOSE_CONTEXT_TOOL, FIND_TOOLS_TOOL,
    QUESTION_FORM_TOOL,
};
use super::doc::{parse_plan_source, PlanDoc};
use super::drafter::{drafter_tool_defs, SAVE_AGENT_TOOL, SAVE_TOOL_TOOL, TRY_TOOL_TOOL};
use super::search::{
    search_tool_defs, CATALOG_TOOLS_TOOL, DESCRIBE_TOOLS_TOOL, SCORE_CANDIDATES_TOOL,
};
use super::{Pipeline, Step};
use crate::tools::{ToolDef, ToolOutcome};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const VALIDATE_PLAN_TOOL: &str = "builtin__validate_plan";

pub const NATIVE_TOOLS: [&str; 11] = [
    VALIDATE_PLAN_TOOL,
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
    let mut defs = vec![ToolDef {
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
    }];
    defs.extend(search_tool_defs());
    defs.extend(compose_tool_defs());
    defs.extend(drafter_tool_defs());
    defs
}

impl Pipeline {
    pub(super) async fn call_native(&self, name: &str, input: Value) -> ToolOutcome {
        let result = match name {
            VALIDATE_PLAN_TOOL => self.validate_plan_input(input).await,
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

pub(super) fn tool_error(message: String) -> ToolOutcome {
    ToolOutcome {
        result: json!({ "error": message }),
        is_error: true,
    }
}
