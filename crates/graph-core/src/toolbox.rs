//! The agent's complete tool catalog: base tools (MCP + built-ins) plus one
//! tool per plan document plus `plan_and_execute`.
//!
//! Plan tools run through [`Pipeline::run_explicit`] (never replans);
//! `plan_and_execute` runs [`Pipeline::run_planned`] (replans on defects).
//! Plan steps invoke the *base* registry, so plans cannot call plans.

use crate::pipeline::{doc::PlanDoc, Pipeline, PipelineError};
use crate::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use std::sync::Arc;

pub const PLAN_TOOL_PREFIX: &str = "plan__";
pub const PLAN_AND_EXECUTE: &str = "plan_and_execute";

pub struct AgentToolbox {
    base: Arc<dyn ToolRegistry>,
    pipeline: Arc<Pipeline>,
    plans: Vec<PlanDoc>,
}

impl AgentToolbox {
    /// `pipeline.registry` must be `base` (or a wrapper of it).
    pub fn new(base: Arc<dyn ToolRegistry>, pipeline: Arc<Pipeline>, plans: Vec<PlanDoc>) -> Self {
        Self {
            base,
            pipeline,
            plans,
        }
    }

    fn plan_and_execute_def(&self) -> ToolDef {
        ToolDef {
            name: PLAN_AND_EXECUTE.to_string(),
            description: "Plan and execute a multi-step data-gathering task: a planner composes \
                          a validated sequence of tool calls with data flowing between steps, \
                          executes it (replanning on failures), and synthesizes an answer. Use \
                          for complex queries needing several dependent tool calls — prefer \
                          calling tools directly for simple lookups."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The complete task, self-contained: include every name, id, timeframe, and constraint the planner needs (it cannot see the conversation)."
                    }
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: None,
        }
    }

    async fn run_plan_tool(&self, doc: &PlanDoc, input: Value) -> ToolOutcome {
        // Validate inputs against the doc's schema; missing/invalid fields
        // come back as a tool error the agent can act on (ask the user,
        // re-call with complete args).
        if let Err(problems) = crate::pipeline::doc::validate_input(doc, &input) {
            return ToolOutcome {
                result: json!({
                    "error": "invalid or missing plan inputs",
                    "problems": problems,
                    "inputSchema": doc.tool_input_schema(),
                }),
                is_error: true,
            };
        }

        let query = format!("Run the '{}' plan", doc.name);
        match self
            .pipeline
            .run_explicit(&query, doc.steps.clone(), doc.solver.clone(), Some(input))
            .await
        {
            Ok(outcome) => ToolOutcome {
                result: json!({"answer": outcome.answer}),
                is_error: false,
            },
            Err(PipelineError::StepFailed {
                step,
                tool,
                message,
            }) => ToolOutcome {
                result: json!({
                    "error": format!("plan '{}' failed at step {step} ({tool}): {message}", doc.identifier),
                }),
                is_error: true,
            },
            Err(e) => ToolOutcome {
                result: json!({"error": e.to_string()}),
                is_error: true,
            },
        }
    }
}

#[async_trait::async_trait]
impl ToolRegistry for AgentToolbox {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut defs = self.base.tools().await?;
        for doc in &self.plans {
            defs.push(ToolDef {
                name: format!("{PLAN_TOOL_PREFIX}{}", doc.identifier),
                description: doc.tool_description(),
                input_schema: doc.tool_input_schema(),
                output_schema: None,
                output_example: None,
                read_only: None,
            });
        }
        defs.push(self.plan_and_execute_def());
        Ok(defs)
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        if name == PLAN_AND_EXECUTE {
            let query = input.get("query").and_then(Value::as_str).ok_or_else(|| {
                ToolError::Transport("plan_and_execute requires a 'query' string".into())
            })?;
            return match self.pipeline.run_planned(query).await {
                Ok(outcome) => Ok(ToolOutcome {
                    result: json!({
                        "answer": outcome.answer,
                        "degraded": outcome.degraded,
                        "steps_executed": outcome.state.results.len(),
                    }),
                    is_error: false,
                }),
                Err(e) => Ok(ToolOutcome {
                    result: json!({"error": e.to_string()}),
                    is_error: true,
                }),
            };
        }
        if let Some(identifier) = name.strip_prefix(PLAN_TOOL_PREFIX) {
            if let Some(doc) = self.plans.iter().find(|d| d.identifier == identifier) {
                return Ok(self.run_plan_tool(doc, input).await);
            }
        }
        self.base.invoke(name, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use graph_config::{ModelChoice, ModelRoles};
    use graph_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, Usage};
    use graph_llm::{ChatProvider, LlmError};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct EchoProvider;

    #[async_trait]
    impl ChatProvider for EchoProvider {
        async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                content: Some("solved".into()),
                tool_calls: vec![],
                structured: None,
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }

    struct BaseRegistry {
        invocations: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl ToolRegistry for BaseRegistry {
        async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
            Ok(vec![ToolDef {
                name: "t__echo".into(),
                description: "echo".into(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            }])
        }
        async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
            self.invocations
                .lock()
                .unwrap()
                .push((name.to_string(), input.clone()));
            Ok(ToolOutcome {
                result: json!({"echoed": input}),
                is_error: false,
            })
        }
    }

    fn toolbox(doc: PlanDoc) -> (AgentToolbox, Arc<BaseRegistry>) {
        let base = Arc::new(BaseRegistry {
            invocations: Mutex::new(Vec::new()),
        });
        let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
        providers.insert("mock".into(), Arc::new(EchoProvider));
        let router = Arc::new(graph_llm::ModelRouter::with_providers(
            providers,
            ModelRoles {
                default: Some(ModelChoice {
                    provider: "mock".into(),
                    model: "m".into(),
                    temperature: None,
                    dimensions: None,
                }),
                ..Default::default()
            },
        ));
        let pipeline = Arc::new(Pipeline {
            router,
            registry: base.clone(),
            events: Arc::new(crate::NullSink),
            shapes: Default::default(),
            user_context: String::new(),
            current_date: "2026-07-09".into(),
            max_attempts: 2,
        });
        (AgentToolbox::new(base.clone(), pipeline, vec![doc]), base)
    }

    fn doc() -> PlanDoc {
        serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: Echo the team name
input_schema:
  type: object
  required: [team]
  properties:
    team: { type: string }
steps:
  - id: E0
    tool_name: t__echo
    input: { q: "{{input.team}}" }
solver:
  query_to_answer: "What did the echo return?"
  data:
    result: "{{E0}}"
"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn exposes_plan_tools_and_plan_and_execute() {
        let (toolbox, _) = toolbox(doc());
        let defs = toolbox.tools().await.unwrap();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"t__echo"));
        assert!(names.contains(&"plan__demo"));
        assert!(names.contains(&"plan_and_execute"));
    }

    #[tokio::test]
    async fn plan_tool_runs_the_pipeline_with_inputs() {
        let (toolbox, base) = toolbox(doc());
        let outcome = toolbox
            .invoke("plan__demo", json!({"team": "Platform"}))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert_eq!(outcome.result["answer"], "solved");
        let invocations = base.invocations.lock().unwrap();
        assert_eq!(
            invocations[0].1,
            json!({"q": "Platform"}),
            "input root rendered"
        );
    }

    #[tokio::test]
    async fn missing_required_inputs_error_with_schema() {
        let (toolbox, base) = toolbox(doc());
        let outcome = toolbox.invoke("plan__demo", json!({})).await.unwrap();
        assert!(outcome.is_error);
        assert!(outcome.result["problems"][0]
            .as_str()
            .unwrap()
            .contains("team"));
        assert!(base.invocations.lock().unwrap().is_empty(), "no steps ran");
    }
}
