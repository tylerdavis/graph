use super::agent::PLANNER_TOOL;
use super::catalog::glob_matches;
use super::gate::StepPath;
use super::native_tools::{native_tool_defs, tool_error};
use super::{DispatchError, Pipeline};
use crate::agent::doc::{
    global_fragments, input_problems, render_system_prompt, AgentDoc, CHAT_AGENT,
};
use crate::agent::task::{run_task_agent, TaskError, TaskSpec, TaskTools};
use crate::tools::{ToolDef, ToolOutcome};
use crate::usage::CallSite;
use async_trait::async_trait;
use graph_llm::types::{ChatMessage, ToolCall, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const AGENT_TOOL_PREFIX: &str = "agent__";

pub const MAX_SUBAGENT_DEPTH: usize = 3;

pub fn named_agent_tool_def(doc: &AgentDoc) -> ToolDef {
    ToolDef {
        name: format!("{AGENT_TOOL_PREFIX}{}", doc.name),
        description: doc.description.clone(),
        input_schema: doc.subagent_input_schema(),
        output_schema: Some(doc.subagent_output_schema()),
        output_example: None,
        read_only: None,
    }
}

impl Pipeline {
    pub(super) fn agent_tool_defs(&self) -> Vec<ToolDef> {
        self.agents
            .iter()
            .filter(|doc| doc.name != CHAT_AGENT)
            .map(named_agent_tool_def)
            .collect()
    }

    pub(super) async fn call_agent(
        &self,
        path: &StepPath,
        name: &str,
        input: Value,
    ) -> Result<ToolOutcome, DispatchError> {
        Ok(self.run_subagent(path, name, input).await?.outcome)
    }

    pub(crate) async fn run_subagent(
        &self,
        path: &StepPath,
        name: &str,
        input: Value,
    ) -> Result<SubagentRun, DispatchError> {
        let failed = |message: String| Ok(SubagentRun::refused(message));
        if self.agent_depth >= MAX_SUBAGENT_DEPTH {
            return failed(format!(
                "agent '{name}' not started: subagents nest at most {MAX_SUBAGENT_DEPTH} deep"
            ));
        }
        let Some(doc) = self.agents.get(name) else {
            return failed(format!("no agent named '{name}'"));
        };
        let problems = input_problems(&doc.subagent_input_schema(), &input);
        if !problems.is_empty() {
            return failed(format!(
                "invalid input for agent '{name}': {}",
                problems.join("; ")
            ));
        }
        let session = BTreeMap::from([
            ("date", self.current_date.clone()),
            ("user", self.user_context.clone()),
        ]);
        let system = match render_system_prompt(doc, &global_fragments(), &session) {
            Ok(system) => system,
            Err(error) => return failed(error),
        };
        let prompt = match &doc.input_schema {
            None => input["prompt"].as_str().unwrap_or_default().to_string(),
            Some(_) => serde_json::to_string_pretty(&input).unwrap_or_default(),
        };
        let tools = self.agent_tools(doc).await;
        let allowed: BTreeSet<String> = tools.iter().map(|tool| tool.name.clone()).collect();
        let spec = TaskSpec {
            model: doc.model.clone(),
            system,
            prompt,
            tools,
            max_iterations: doc.max_iterations,
            output_schema: doc.output_schema.clone(),
        };
        let mut child = self.clone();
        child.agent_depth += 1;
        let agent_path = path.nested(&format!("{AGENT_TOOL_PREFIX}{name}"), None);
        let empty = Map::new();
        let tools = AgentTools {
            pipeline: &child,
            path: &agent_path,
            scope: &empty,
            allowed: &allowed,
        };
        let site = CallSite::role(doc.model.clone())
            .at(path.to_string())
            .in_plans(&self.call_stack);
        match run_task_agent(&spec, &self.router, &site, self.events.as_ref(), &tools).await {
            Ok(outcome) => Ok(SubagentRun {
                outcome: if outcome.final_ {
                    ToolOutcome {
                        result: outcome.output.clone(),
                        is_error: false,
                    }
                } else {
                    ToolOutcome {
                        result: json!({
                            "error": format!("agent '{name}' ran out of rounds before answering"),
                            "partial": outcome.output,
                        }),
                        is_error: true,
                    }
                },
                output: outcome.output,
                messages: outcome.messages,
                final_: outcome.final_,
            }),
            Err(TaskError::Failed(message)) => failed(format!("agent '{name}' failed: {message}")),
            Err(TaskError::Aborted(error)) => Err(DispatchError::Aborted { error }),
        }
    }

    async fn agent_tools(&self, doc: &AgentDoc) -> Vec<ToolSpec> {
        let mut candidates = self.registry.tools().await.unwrap_or_default();
        candidates.extend(native_tool_defs());
        candidates.extend(self.callable_plan_defs());
        let mut specs: Vec<ToolSpec> = candidates
            .into_iter()
            .filter(|tool| tool.name != PLANNER_TOOL)
            .filter(|tool| {
                doc.tools
                    .iter()
                    .any(|pattern| glob_matches(pattern, &tool.name))
            })
            .map(to_spec)
            .collect();
        specs.extend(
            doc.subagents
                .iter()
                .filter_map(|target| self.agents.get(target))
                .map(|target| to_spec(named_agent_tool_def(target))),
        );
        specs
    }
}

pub struct SubagentRun {
    pub outcome: ToolOutcome,
    pub output: Value,
    pub messages: Vec<ChatMessage>,
    pub final_: bool,
}

impl SubagentRun {
    fn refused(message: String) -> Self {
        Self {
            outcome: tool_error(message),
            output: Value::Null,
            messages: Vec::new(),
            final_: false,
        }
    }
}

fn to_spec(tool: ToolDef) -> ToolSpec {
    ToolSpec {
        name: tool.name,
        description: tool.description,
        input_schema: tool.input_schema,
    }
}

struct AgentTools<'a> {
    pipeline: &'a Pipeline,
    path: &'a StepPath,
    scope: &'a Map<String, Value>,
    allowed: &'a BTreeSet<String>,
}

#[async_trait]
impl TaskTools for AgentTools<'_> {
    async fn execute(&self, calls: &[ToolCall], round: u32) -> Result<Vec<ChatMessage>, TaskError> {
        let (offered, refused): (Vec<ToolCall>, Vec<ToolCall>) = calls
            .iter()
            .cloned()
            .partition(|call| self.allowed.contains(&call.name));
        let mut results = self
            .pipeline
            .execute_agent_tools(&offered, self.path, round, self.scope)
            .await?;
        results.extend(refused.into_iter().map(|call| ChatMessage::ToolResult {
            tool_call_id: call.id,
            content: json!({ "error": format!("tool '{}' is not available to this agent", call.name) }),
            is_error: true,
        }));
        let position = |message: &ChatMessage| match message {
            ChatMessage::ToolResult { tool_call_id, .. } => calls
                .iter()
                .position(|call| &call.id == tool_call_id)
                .unwrap_or(usize::MAX),
            _ => usize::MAX,
        };
        results.sort_by_key(position);
        Ok(results)
    }
}
