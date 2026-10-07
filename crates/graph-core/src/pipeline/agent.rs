//! The `agent` control step: a bounded tool-calling loop that produces
//! structured output. Intercepted by the executor — never dispatched to a
//! tool registry.
//!
//! The agent is a functional component: it takes a prompt, calls tools,
//! reasons over results, and produces structured output conforming to a
//! declared schema — or reports `final: false` when its round budget ran
//! out first. It never invents a conforming result.

use super::catalog::glob_matches;
use super::gate::StepPath;
use super::{ExecutionEnd, Pipeline, RunState, Step};
use crate::agent::task::{run_task_agent, TaskError, TaskSpec, TaskTools};
use crate::template::{render_str, RenderError, Roots};
use crate::tools::{ToolDef, ToolRegistry};
use crate::usage::CallSite;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

use graph_llm::types::{ChatMessage, ToolCall, ToolSpec};

pub use crate::agent::task::ToolCallEntry;

/// Reserved step tool name.
pub const AGENT_TOOL: &str = "agent";

/// The planner tool, never reachable from inside an agent.
pub(super) const PLANNER_TOOL: &str = "plan_and_execute";

/// The agent step's input, parsed from the RAW (unrendered) step input:
/// `prompt` and `systemPrompt` render against the step's scope when the
/// loop starts, not at parse time.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSpec {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    // camelCase is the canonical prompt surface; the snake_case aliases
    // match how humans author plan YAML (as `Step::tool_name` does).
    #[serde(default, alias = "system_prompt")]
    pub system_prompt: Option<String>,
    #[serde(default = "default_max_iterations", alias = "max_iterations")]
    pub max_iterations: u32,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(alias = "output_schema")]
    pub output_schema: Value,
}

fn default_max_iterations() -> u32 {
    8
}

/// The agent's structured result envelope.
#[derive(Debug, Serialize)]
pub struct AgentResult {
    pub output: Value,
    pub iterations: u32,
    pub tools_called: Vec<ToolCallEntry>,
    #[serde(rename = "final")]
    pub final_: bool,
}

/// Everything the agent needs to know about *how it is being run*.
///
/// This is the whole point of the `agent` step being turn-key: a plan author
/// writes a task prompt and nothing else, and the harness tells the model
/// about its budget, when to stop, and how to be efficient. Guidance that
/// would otherwise have to be repeated in every plan belongs here.
///
/// Kept byte-identical across the rounds of a run — the step's schema and
/// budget are appended, and both are fixed for the life of the loop — so the
/// whole thing sits inside the cached prefix. Per-round state must never go
/// in here; it goes at the end of the message list instead.
const BUILTIN_SYSTEM_PROMPT: &str = include_str!("prompts/agent_system.md").trim_ascii_end();

/// Heading the step's `output_schema` is rendered under in the system prompt.
const SCHEMA_HEADING: &str = "\n\n# Output schema\n\nYour final answer must be \
                              a JSON object matching this schema:\n\n";

/// The agent step as described to the planner.
pub fn agent_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: AGENT_TOOL.to_string(),
        description: "Run a tool-calling loop that calls tools and reasons over \
                      results to produce structured output. Use when the task \
                      requires multiple rounds of tool use and reasoning — when \
                      WHICH tool to call next depends on what an earlier call \
                      returned. Returns output conforming to outputSchema, or \
                      final: false when the round budget ran out first. \
                      plan_and_execute is not available as a tool."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["prompt", "outputSchema"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The task description. May use templates like {{E0.results}}."
                },
                "model": {
                    "type": "string",
                    "description": "A configured model role, standard or custom. Defaults to the chat role."
                },
                "systemPrompt": {
                    "type": "string",
                    "description": "Extra system prompt guidance. May use templates like the prompt."
                },
                "maxIterations": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum inference rounds — every round is one model call, including the one that produces the final answer. Default: 8."
                },
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "description": "Tool names or wildcard patterns (e.g. linear__*) to expose. Every pattern must resolve to at least one tool. Omit for all available tools; an empty list is an error, not 'no tools'."
                },
                "outputSchema": {
                    "description": "JSON Schema for the agent's structured output. Must be type object."
                }
            }
        }),
        output_schema: None,
        output_example: Some(json!({
            "output": {"classifications": [{"issue_id": "123", "severity": "high", "reason": "…"}]},
            "iterations": 3,
            "tools_called": [{"tool": "linear__list_issues", "round": 1}],
            "final": true
        })),
        read_only: None,
    }
}

// ── Wildcard tool resolution ───────────────────────────────────────────────

/// Resolve tool names/patterns against the registry.
///
/// Patterns use simple glob matching:
/// - `linear__*` → all tools starting with `linear__`
/// - `linear__list_*` → tools starting with `linear__list_`
/// - `linear__get_issue` → exact match
/// - Omit `tools` → return all tools
///
/// A pattern that matches nothing is an error naming that pattern —
/// silently dropping it would let a typo quietly shrink the catalogue.
pub async fn resolve_tools(
    patterns: &[String],
    registry: &dyn ToolRegistry,
    plans: &[super::doc::PlanDoc],
) -> Result<Vec<ToolDef>, String> {
    let mut all_tools = registry
        .tools()
        .await
        .map_err(|e| format!("agent tool discovery failed: {e}"))?;

    // The pipeline's registry is the base catalog (MCP + user + builtin);
    // plan tools live in `Pipeline::plans` and are layered on by the chat
    // agent's toolbox. Agents compose with plans instead of
    // `plan_and_execute`, so they need them here too. Dispatch already
    // routes `plan__*` to `call_plan`.
    for doc in plans {
        all_tools.push(ToolDef {
            name: format!("{}{}", crate::toolbox::PLAN_TOOL_PREFIX, doc.identifier),
            description: doc.tool_description(),
            input_schema: doc.tool_input_schema(),
            output_schema: None,
            output_example: None,
            read_only: None,
        });
    }

    if patterns.is_empty() {
        return Ok(all_tools);
    }

    // Keyed and ordered by name: overlapping patterns dedupe, and the
    // catalogue the model sees is byte-identical run to run (prompt
    // caching, reproducible traces), like every other catalog surface.
    let mut matched: BTreeMap<String, ToolDef> = BTreeMap::new();
    let mut unmatched: Vec<&str> = Vec::new();
    for pattern in patterns {
        let mut hit = false;
        for tool in &all_tools {
            if glob_matches(pattern, &tool.name) {
                matched.insert(tool.name.clone(), tool.clone());
                hit = true;
            }
        }
        if !hit {
            unmatched.push(pattern);
        }
    }

    if !unmatched.is_empty() {
        return Err(format!(
            "`tools` pattern(s) matched no tool in the catalogue: {}",
            unmatched.join(", ")
        ));
    }

    Ok(matched.into_values().collect())
}

// ── Static validation ──────────────────────────────────────────────────────

/// Validate an agent step's raw input at plan load time.
pub fn validate_agent_input(
    input: &Map<String, Value>,
    seen: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let spec: AgentSpec = match serde_json::from_value(Value::Object(input.clone())) {
        Ok(spec) => spec,
        Err(e) => {
            problems.push(format!("step {step_id}: invalid agent input: {e}"));
            return;
        }
    };

    if spec.max_iterations < 1 {
        problems.push(format!(
            "step {step_id}: `maxIterations` must be at least 1"
        ));
    }

    // An explicit empty list reads as "no tools" but would grant the whole
    // catalogue (an omitted `tools` and an empty one are the same value
    // downstream). Refuse it rather than pick a meaning.
    if spec.tools.as_deref().is_some_and(<[String]>::is_empty) {
        problems.push(format!(
            "step {step_id}: `tools` is empty — omit it to expose the whole \
             catalogue, or name at least one tool or pattern"
        ));
    }

    // Validate output_schema is valid JSON Schema
    if jsonschema::validator_for(&spec.output_schema).is_err() {
        problems.push(format!(
            "step {step_id}: `outputSchema` is not valid JSON Schema"
        ));
    }

    // output_schema must have type: "object"
    if let Some(schema_type) = spec.output_schema.get("type").and_then(Value::as_str) {
        if schema_type != "object" {
            problems.push(format!(
                "step {step_id}: `outputSchema` must have type \"object\""
            ));
        }
    } else {
        // No type specified — check for properties (implies object)
        if spec.output_schema.get("properties").is_none() {
            problems.push(format!(
                "step {step_id}: `outputSchema` must have type \"object\" or declare properties"
            ));
        }
    }

    // Both rendered fields carry templates, so both are reference-checked.
    super::check_templates(&Value::String(spec.prompt), seen, step_id, problems);
    if let Some(system) = spec.system_prompt {
        super::check_templates(&Value::String(system), seen, step_id, problems);
    }
}

// ── Agent loop execution ───────────────────────────────────────────────────

/// How an agent step ended badly. Mapped to `ExecutionEnd` at the top
/// level and to `BodyFail` inside a control-step body.
pub(super) enum AgentFail {
    Failed(String),
    /// Data ran out while rendering the prompt — degrades, never replans.
    /// Carries the render error itself so a body can hand it to
    /// `BodyFail::Render`, which is where that classification lives.
    Empty(RenderError),
    /// The execution gate aborted an inner tool call. Carries the failing
    /// tool's error when the abort came from `on_tool_error`.
    Aborted(Option<Value>),
}

/// A completed agent run: the result envelope plus the number of real
/// tool dispatches it performed, for `steps_executed` accounting.
pub(super) struct AgentRun {
    pub result: Value,
    pub tool_calls: usize,
}

impl Pipeline {
    /// Top-level `agent` step: runs against the plan's results map.
    pub(super) async fn run_agent(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        let path = StepPath::top(&step.id);
        let scope = state.results.clone();
        match self.run_agent_scoped(&path, &step.input, &scope).await {
            Ok(run) => {
                // Inner tool calls are real dispatches at depth — count
                // them the way body calls are counted.
                state.branch_steps_executed += run.tool_calls;
                // One summary line, matching `decide → then` / `map: N items`.
                let rounds = run
                    .result
                    .get("iterations")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let finished = run
                    .result
                    .get("final")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                state.push_bus(
                    &step.id,
                    super::state::BusKind::Info,
                    format!(
                        "agent: {rounds} round(s), {} tool call(s){}",
                        run.tool_calls,
                        if finished {
                            ""
                        } else {
                            " — budget exhausted"
                        }
                    ),
                );
                Ok(run.result)
            }
            Err(AgentFail::Empty(error)) => Err(ExecutionEnd::Empty {
                step: step.id.clone(),
                message: error.to_string(),
            }),
            Err(AgentFail::Failed(message)) => Err(ExecutionEnd::Failed {
                step: step.id.clone(),
                tool: AGENT_TOOL.to_string(),
                message,
            }),
            Err(AgentFail::Aborted(error)) => Err(ExecutionEnd::Aborted {
                step: step.id.clone(),
                error,
            }),
        }
    }

    /// The agent loop, rendered against an arbitrary scope so it works
    /// identically at the top level and inside a `route`/`map`/`reduce`
    /// body (where the scope carries `item`/`index`/`accumulator`).
    ///
    /// The agent step itself is never gated — it is a control step, and
    /// control-step evaluation is never gated. Every *inner* tool call
    /// goes through `Pipeline::dispatch`, so it is gated, evented, cycle-
    /// checked, and able to reach `plan__*` exactly like any other call.
    pub(super) async fn run_agent_scoped(
        &self,
        path: &StepPath,
        raw_input: &Map<String, Value>,
        scope: &Map<String, Value>,
    ) -> Result<AgentRun, AgentFail> {
        let spec: AgentSpec = serde_json::from_value(Value::Object(raw_input.clone()))
            .map_err(|e| AgentFail::Failed(format!("invalid agent input: {e}")))?;

        let roots = Roots::new(scope);
        let render = |text: &str| match render_str(text, &roots) {
            Ok(rendered) => Ok(rendered),
            Err(e @ RenderError::EmptyData { .. }) => Err(AgentFail::Empty(e)),
            Err(e) => Err(AgentFail::Failed(e.to_string())),
        };
        let prompt = render(&spec.prompt)?;
        // The schema goes in the prompt, not the request's `response_schema`.
        // Providers implement that natively by forcing a synthetic structured
        // tool (`tool_choice: {type: "tool"}`), which would make the agent's
        // real tools unreachable — so a tool-calling loop has to show the
        // model the shape as text instead. Without it the model is guessing
        // field names against a schema it cannot see, and each wrong guess
        // fails validation and consumes a round; an agent that always
        // exhausts its budget is what that looks like from outside.
        //
        // Stable for the life of the loop, and ahead of the conversation, so
        // it sits inside the cached prefix and costs ~nothing after round 1.
        let schema_text = serde_json::to_string_pretty(&spec.output_schema)
            .unwrap_or_else(|_| spec.output_schema.to_string());
        let mut system = format!("{BUILTIN_SYSTEM_PROMPT}{SCHEMA_HEADING}{schema_text}");
        if let Some(extra) = &spec.system_prompt {
            system.push_str("\n\n");
            system.push_str(&render(extra)?);
        }

        // Validation rejects an empty `tools` list, but a plan can reach
        // the executor unvalidated (a gate-injected draft, a direct API
        // caller), and "all tools" is the wrong guess to make here.
        let patterns = match spec.tools.as_deref() {
            Some([]) => {
                return Err(AgentFail::Failed(
                    "`tools` is empty — omit it to expose the whole catalogue, \
                     or name at least one tool or pattern"
                        .to_string(),
                ))
            }
            Some(patterns) => patterns,
            None => &[],
        };
        let tools = resolve_tools(patterns, self.registry.as_ref(), &self.plans)
            .await
            .map_err(AgentFail::Failed)?;

        // `plan_and_execute` is never offered inside an agent: nested
        // planning loops have no coherent cost boundary. `plan__*` stays,
        // and works, because inner calls go through dispatch. Advertising
        // is only half of it — `execute_agent_tools` refuses the name too,
        // since dispatch would happily route it.
        let tool_specs: Vec<ToolSpec> = tools
            .into_iter()
            .filter(|t| t.name != PLANNER_TOOL)
            .map(|t| ToolSpec {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
            })
            .collect();

        let model_name = spec.model.as_deref().unwrap_or("chat");

        // Every round *and* every schema-repair pass this agent triggers bills
        // to this step. Repair is the easy one to lose: it is uncapped, runs
        // on a different model, and is invisible in the round count.
        let site = CallSite::role(model_name)
            .at(path.to_string())
            .in_plans(&self.call_stack);

        let task = TaskSpec {
            model: model_name.to_string(),
            system,
            prompt,
            tools: tool_specs,
            max_iterations: Some(spec.max_iterations),
            output_schema: Some(spec.output_schema),
        };
        let step_tools = StepTools {
            pipeline: self,
            path,
            scope,
        };
        let outcome = run_task_agent(
            &task,
            &self.router,
            &site,
            self.events.as_ref(),
            &step_tools,
        )
        .await
        .map_err(|error| match error {
            TaskError::Failed(message) => AgentFail::Failed(message),
            TaskError::Aborted(error) => AgentFail::Aborted(error),
        })?;
        Ok(AgentRun {
            result: envelope(
                outcome.output,
                outcome.iterations,
                &outcome.tools_called,
                outcome.final_,
            ),
            tool_calls: outcome.tools_called.len(),
        })
    }

    /// Run one round's tool calls in parallel through `Pipeline::dispatch`.
    ///
    /// A tool *error* is not a step failure: it returns into the loop as an
    /// error result so the agent can explain or work around it. A gate
    /// *abort* is a hard stop and propagates.
    pub(super) async fn execute_agent_tools(
        &self,
        calls: &[ToolCall],
        path: &StepPath,
        round: u32,
        scope: &Map<String, Value>,
    ) -> Result<Vec<ChatMessage>, TaskError> {
        let futures = calls.iter().map(|call| {
            // Nested, not rebuilt from the step id: an agent inside a map
            // body must keep its item segment (E1/do.2/agent.3/tool), or
            // concurrent items report indistinguishable paths.
            let call_path = path.nested(&format!("agent.{round}"), Some(call.name.as_str()));
            async move {
                // Advertising `plan_and_execute` is suppressed, but
                // `dispatch` routes the bare name, so a model that emits it
                // anyway would get nested planning. Refuse it here, as an
                // error the agent can recover from.
                if call.name == PLANNER_TOOL {
                    return (
                        call,
                        Err(super::DispatchError::Failed(format!(
                            "{PLANNER_TOOL} is not available inside an agent step; \
                             call a plan (plan__*) instead"
                        ))),
                    );
                }
                let outcome = self
                    .dispatch(&call_path, &call.name, call.arguments.clone(), scope)
                    .await;
                (call, outcome)
            }
        });

        let settled = futures::future::join_all(futures).await;

        // Drain every in-flight call before honouring an abort, matching
        // `map`'s failure semantics.
        let mut abort: Option<Option<Value>> = None;
        let mut messages = Vec::with_capacity(settled.len());
        for (call, outcome) in settled {
            match outcome {
                Ok(result) => messages.push(ChatMessage::ToolResult {
                    tool_call_id: call.id.clone(),
                    content: result,
                    is_error: false,
                }),
                Err(super::DispatchError::Failed(message)) => {
                    messages.push(ChatMessage::ToolResult {
                        tool_call_id: call.id.clone(),
                        content: json!({ "error": message }),
                        is_error: true,
                    });
                }
                Err(super::DispatchError::Aborted { error }) => {
                    abort.get_or_insert(error);
                }
            }
        }

        match abort {
            Some(error) => Err(TaskError::Aborted(error)),
            None => Ok(messages),
        }
    }
}

struct StepTools<'a> {
    pipeline: &'a Pipeline,
    path: &'a StepPath,
    scope: &'a Map<String, Value>,
}

#[async_trait]
impl TaskTools for StepTools<'_> {
    async fn execute(&self, calls: &[ToolCall], round: u32) -> Result<Vec<ChatMessage>, TaskError> {
        self.pipeline
            .execute_agent_tools(calls, self.path, round, self.scope)
            .await
    }
}

/// Build the agent's result envelope.
fn envelope(output: Value, iterations: u32, tools_called: &[ToolCallEntry], final_: bool) -> Value {
    serde_json::to_value(AgentResult {
        output,
        iterations,
        tools_called: tools_called.to_vec(),
        final_,
    })
    .unwrap_or_else(|e| json!({ "error": format!("agent serialization failed: {e}") }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolDef, ToolError, ToolOutcome};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Arc;

    // ── Wildcard resolution tests ──────────────────────────────────────

    #[tokio::test]
    async fn wildcard_matches_all_tools_with_prefix() {
        let registry = Arc::new(MockRegistry::new(vec![
            ToolDef {
                name: "linear__list_issues".into(),
                description: "list issues".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "linear__get_issue".into(),
                description: "get issue".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "user__git_log".into(),
                description: "git log".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
        ]));

        let tools = resolve_tools(&["linear__*".to_string()], &*registry, &[])
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"linear__list_issues"));
        assert!(names.contains(&"linear__get_issue"));
        assert!(!names.contains(&"user__git_log"));
    }

    #[tokio::test]
    async fn wildcard_matches_partial_suffix() {
        let registry = Arc::new(MockRegistry::new(vec![
            ToolDef {
                name: "linear__list_issues".into(),
                description: "list issues".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "linear__list_teams".into(),
                description: "list teams".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "linear__get_issue".into(),
                description: "get issue".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
        ]));

        let tools = resolve_tools(&["linear__list_*".to_string()], &*registry, &[])
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"linear__list_issues"));
        assert!(names.contains(&"linear__list_teams"));
        assert!(!names.contains(&"linear__get_issue"));
    }

    #[tokio::test]
    async fn exact_match_selects_single_tool() {
        let registry = Arc::new(MockRegistry::new(vec![
            ToolDef {
                name: "linear__list_issues".into(),
                description: "list issues".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "linear__list_teams".into(),
                description: "list teams".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
        ]));

        let tools = resolve_tools(&["linear__list_teams".to_string()], &*registry, &[])
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "linear__list_teams");
    }

    #[tokio::test]
    async fn omit_tools_returns_all() {
        let registry = Arc::new(MockRegistry::new(vec![
            ToolDef {
                name: "linear__list_issues".into(),
                description: "list issues".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "user__git_log".into(),
                description: "git log".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
        ]));

        let tools = resolve_tools(&[], &*registry, &[]).await.unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[tokio::test]
    async fn an_unknown_pattern_is_reported_not_dropped() {
        let registry = Arc::new(MockRegistry::new(vec![ToolDef {
            name: "linear__list_issues".into(),
            description: "list issues".into(),
            input_schema: json!({}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }]));

        let err = resolve_tools(&["nonexistent__*".to_string()], &*registry, &[])
            .await
            .unwrap_err();
        assert!(err.contains("nonexistent__*"), "{err}");
    }

    #[tokio::test]
    async fn empty_tool_list_is_valid_when_omitted() {
        // When tools is None/omitted, an empty result is fine — all tools are used.
        let registry = Arc::new(MockRegistry::new(vec![]));
        let tools = resolve_tools(&[], &*registry, &[]).await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn deduplicates_across_patterns() {
        let registry = Arc::new(MockRegistry::new(vec![ToolDef {
            name: "linear__list_issues".into(),
            description: "list issues".into(),
            input_schema: json!({}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }]));

        // Both patterns match the same tool
        let tools = resolve_tools(
            &["linear__*".to_string(), "linear__list_issues".to_string()],
            &*registry,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(tools.len(), 1, "should deduplicate");
    }

    #[tokio::test]
    async fn resolved_tools_come_back_in_a_stable_order() {
        // The catalogue the model sees must not reorder between runs.
        let registry = Arc::new(MockRegistry::new(vec![
            ToolDef {
                name: "linear__list_teams".into(),
                description: "d".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "linear__get_issue".into(),
                description: "d".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "linear__add_comment".into(),
                description: "d".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
        ]));

        let names: Vec<String> = resolve_tools(&["linear__*".to_string()], &*registry, &[])
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(
            names,
            [
                "linear__add_comment",
                "linear__get_issue",
                "linear__list_teams"
            ]
        );
    }

    #[tokio::test]
    async fn excludes_plan_and_execute_from_tools() {
        let registry = Arc::new(MockRegistry::new(vec![
            ToolDef {
                name: "linear__list_issues".into(),
                description: "list issues".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
            ToolDef {
                name: "plan_and_execute".into(),
                description: "plan and execute".into(),
                input_schema: json!({}),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
        ]));

        let tools = resolve_tools(&[], &*registry, &[]).await.unwrap();
        // All tools are returned; plan_and_execute is excluded later
        // when building tool_specs for the LLM
        assert_eq!(tools.len(), 2);
    }

    // ── Validation tests ───────────────────────────────────────────────

    #[test]
    fn validate_rejects_zero_max_iterations() {
        let input: Map<String, Value> = json!({
            "prompt": "test",
            "maxIterations": 0,
            "outputSchema": {"type": "object"}
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(
            problems.iter().any(|p| p.contains("at least 1")),
            "{problems:?}"
        );
    }

    #[test]
    fn validate_rejects_non_object_output_schema() {
        let input: Map<String, Value> = json!({
            "prompt": "test",
            "outputSchema": {"type": "string"}
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(problems.iter().any(|p| p.contains("type")), "{problems:?}");
    }

    #[test]
    fn validate_accepts_object_output_schema_with_properties() {
        let input: Map<String, Value> = json!({
            "prompt": "test",
            "outputSchema": {
                "type": "object",
                "properties": {"name": {"type": "string"}}
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn validate_rejects_forward_template_references() {
        let input: Map<String, Value> = json!({
            "prompt": "{{E5.values}}",
            "outputSchema": {"type": "object"}
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(problems.iter().any(|p| p.contains("E5")), "{problems:?}");
    }

    #[test]
    fn validate_accepts_backward_template_references() {
        let input: Map<String, Value> = json!({
            "prompt": "{{E0.results}}",
            "outputSchema": {"type": "object"}
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn validate_rejects_an_explicitly_empty_tool_list() {
        // `tools: []` reads as "no tools" but resolves to the whole
        // catalogue, so it must not be a silent success either way.
        let input: Map<String, Value> = json!({
            "prompt": "test",
            "outputSchema": {"type": "object"},
            "tools": []
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(
            problems.iter().any(|p| p.contains("`tools` is empty")),
            "{problems:?}"
        );
    }

    #[test]
    fn validate_rejects_a_non_string_prompt_at_parse_time() {
        let input: Map<String, Value> = json!({
            "prompt": 12345,
            "outputSchema": {"type": "object"}
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(
            problems.iter().any(|p| p.contains("invalid agent input")),
            "{problems:?}"
        );
    }

    #[test]
    fn validate_checks_system_prompt_references_too() {
        let input: Map<String, Value> = json!({
            "prompt": "fine",
            "systemPrompt": "context: {{E9.values}}",
            "outputSchema": {"type": "object"}
        })
        .as_object()
        .unwrap()
        .clone();
        let mut problems = Vec::new();
        validate_agent_input(&input, &["input", "E0"], "E1", &mut problems);
        assert!(problems.iter().any(|p| p.contains("E9")), "{problems:?}");
    }

    #[test]
    fn validate_rejects_unknown_fields() {
        let input: Map<String, Value> = json!({
            "prompt": "test",
            "outputSchema": {"type": "object"},
            "unknownField": "bad"
        })
        .as_object()
        .unwrap()
        .clone();
        let result: Result<AgentSpec, _> = serde_json::from_value(Value::Object(input));
        assert!(result.is_err(), "should reject unknown fields");
    }

    // ── Tool definition test ───────────────────────────────────────────

    /// The planner writes plans by copying the tool def's schema. If the
    /// schema and the serde contract disagree, every plan the planner
    /// writes is unparseable — so build a document from the def's OWN
    /// required names and prove serde accepts it.
    #[test]
    fn planner_schema_and_serde_contract_cannot_drift() {
        let def = agent_tool_def();
        let required: Vec<&str> = def.input_schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .map(|v| v.as_str().expect("string"))
            .collect();

        let mut doc = Map::new();
        for name in &required {
            let value = match *name {
                "prompt" => json!("do the thing"),
                _ => json!({"type": "object", "properties": {}}),
            };
            doc.insert((*name).to_string(), value);
        }
        serde_json::from_value::<AgentSpec>(Value::Object(doc))
            .expect("tool def's required fields must deserialize into AgentSpec");

        // Every advertised optional property must be accepted too.
        for name in def.input_schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
        {
            let mut doc = Map::new();
            doc.insert("prompt".into(), json!("x"));
            doc.insert(
                "outputSchema".into(),
                json!({"type": "object", "properties": {}}),
            );
            let value = match name.as_str() {
                "prompt" => json!("x"),
                "outputSchema" => json!({"type": "object", "properties": {}}),
                "maxIterations" => json!(3),
                "tools" => json!(["user__x"]),
                _ => json!("x"),
            };
            doc.insert(name.clone(), value);
            serde_json::from_value::<AgentSpec>(Value::Object(doc))
                .unwrap_or_else(|e| panic!("advertised property `{name}` rejected by serde: {e}"));
        }
    }

    #[test]
    fn snake_case_aliases_are_accepted_for_yaml_authors() {
        let spec: AgentSpec = serde_json::from_value(json!({
            "prompt": "x",
            "output_schema": {"type": "object", "properties": {}},
            "max_iterations": 2,
            "system_prompt": "extra"
        }))
        .expect("snake_case aliases must parse");
        assert_eq!(spec.max_iterations, 2);
        assert_eq!(spec.system_prompt.as_deref(), Some("extra"));
    }

    #[tokio::test]
    async fn a_pattern_matching_nothing_is_named_in_the_error() {
        let registry = Arc::new(MockRegistry::new(vec![ToolDef {
            name: "user__git_log".into(),
            description: "d".into(),
            input_schema: json!({}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }]));
        // One good pattern, one typo: must still fail, naming the typo.
        let err = resolve_tools(
            &["user__*".to_string(), "linear__*".to_string()],
            &*registry,
            &[],
        )
        .await
        .unwrap_err();
        assert!(err.contains("linear__*"), "{err}");
        assert!(!err.contains("user__*"), "{err}");
    }

    #[test]
    fn agent_tool_def_advertises_camel_case_prompt_surface() {
        let def = agent_tool_def();
        assert_eq!(def.name, "agent");
        assert!(def.description.contains("structured output"));
        let required: Vec<&str> = def.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["prompt", "outputSchema"]);
        let props = def.input_schema["properties"].as_object().unwrap();
        for name in ["outputSchema", "maxIterations", "systemPrompt"] {
            assert!(props.contains_key(name), "missing camelCase `{name}`");
        }
        for name in ["output_schema", "max_iterations", "system_prompt"] {
            assert!(
                !props.contains_key(name),
                "snake_case `{name}` leaked into the prompt surface"
            );
        }
    }

    // ── Mock registry ──────────────────────────────────────────────────

    struct MockRegistry {
        tools: Vec<ToolDef>,
    }

    impl MockRegistry {
        fn new(tools: Vec<ToolDef>) -> Self {
            Self { tools }
        }
    }

    #[async_trait]
    impl ToolRegistry for MockRegistry {
        async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
            Ok(self.tools.clone())
        }

        async fn invoke(&self, _name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome {
                result: json!({"ok": true}),
                is_error: false,
            })
        }
    }
}
