use super::drafting::{draft_step_tool_def, DRAFT_STEP_TOOL};
use super::{prompts, Pipeline, Step};
use crate::tools::{ToolDef, ToolOutcome};
use serde_json::{json, Value};

pub const CATALOG_OUTLINE_TOOL: &str = "builtin__catalog_outline";

pub const VALIDATE_PLAN_TOOL: &str = "builtin__validate_plan";

pub const NATIVE_TOOLS: [&str; 3] = [CATALOG_OUTLINE_TOOL, DRAFT_STEP_TOOL, VALIDATE_PLAN_TOOL];

pub fn is_native_tool(name: &str) -> bool {
    NATIVE_TOOLS.contains(&name)
}

pub fn native_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: CATALOG_OUTLINE_TOOL.to_string(),
            description: "Summarizes what a plan can use, in sections: MCP servers with their \
                          descriptions, tool packs, builtin tools, agents, user tools and plans. \
                          Names and one-line summaries only, never schemas."
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
        draft_step_tool_def(),
        ToolDef {
            name: VALIDATE_PLAN_TOOL.to_string(),
            description: "Statically validates a list of plan steps: ids, references between \
                          steps, control-step grammar and templates. Returns every problem found."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "steps": {"type": "array", "items": {"type": "object"}, "description": "The plan's steps"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["valid", "problems"],
                "properties": {
                    "valid": {"type": "boolean"},
                    "problems": {"type": "array", "items": {"type": "string"}}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
    ]
}

impl Pipeline {
    pub(super) async fn catalog_outline(&self) -> String {
        let mut names: Vec<String> = self
            .registry
            .tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        names.extend(self.callable_plan_defs().into_iter().map(|tool| tool.name));
        let servers = self.registry.servers().await;
        let agents: Vec<(String, String)> = self
            .agents
            .iter()
            .map(|doc| (doc.name.clone(), doc.description.clone()))
            .collect();
        prompts::outliner_catalog(&names, &servers, &agents)
    }

    pub(super) async fn call_native(&self, name: &str, input: Value) -> ToolOutcome {
        let result = match name {
            CATALOG_OUTLINE_TOOL => Ok(json!({ "text": self.catalog_outline().await })),
            DRAFT_STEP_TOOL => self.draft_step(input).await,
            VALIDATE_PLAN_TOOL => self.validate_steps(input),
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

    fn validate_steps(&self, input: Value) -> Result<Value, String> {
        let steps: Vec<Step> = serde_json::from_value(input["steps"].clone())
            .map_err(|e| format!("invalid steps: {e}"))?;
        let problems = self.validate_plan(&steps).err().unwrap_or_default();
        Ok(json!({ "valid": problems.is_empty(), "problems": problems }))
    }
}

pub(super) fn tool_error(message: String) -> ToolOutcome {
    ToolOutcome {
        result: json!({ "error": message }),
        is_error: true,
    }
}
