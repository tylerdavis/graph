//! Pipeline tests over scripted LLM + tool mocks.

use super::*;
use crate::tools::{ToolDef, ToolError};
use crate::NullSink;
use async_trait::async_trait;
use graph_config::{ModelChoice, ModelRoles};
use graph_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, Usage};
use graph_llm::{ChatProvider, LlmError};
use serde_json::{json, Value};
use std::sync::Mutex;

struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl ChatProvider for ScriptedProvider {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests.lock().unwrap().push(req);
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(LlmError::Parse("script exhausted".into()));
        }
        Ok(responses.remove(0))
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        use futures::StreamExt;
        let response = self.chat(req).await?;
        Ok(
            futures::stream::iter(vec![Ok(graph_llm::types::StreamEvent::Completed(response))])
                .boxed(),
        )
    }
}

fn structured(value: Value) -> ChatResponse {
    ChatResponse {
        content: None,
        tool_calls: vec![],
        structured: Some(value),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn text(answer: &str) -> ChatResponse {
    ChatResponse {
        content: Some(answer.to_string()),
        tool_calls: vec![],
        structured: None,
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

/// Registry with two tools: `t__search` returns a canned value, `t__issues`
/// echoes its input under `got`.
struct MockRegistry {
    search_result: Value,
    invocations: Mutex<Vec<(String, Value)>>,
    fail_tools: Vec<String>,
}

#[async_trait]
impl ToolRegistry for MockRegistry {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(["t__search", "t__issues"]
            .iter()
            .map(|name| ToolDef {
                name: name.to_string(),
                description: "test tool".into(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            })
            .collect())
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        self.invocations
            .lock()
            .unwrap()
            .push((name.to_string(), input.clone()));
        if self.fail_tools.iter().any(|t| t == name) {
            return Ok(ToolOutcome {
                result: json!({"error": "boom"}),
                is_error: true,
            });
        }
        let result = match name {
            "t__search" => self.search_result.clone(),
            "t__issues" => json!({"got": input}),
            other => return Err(ToolError::Unknown(other.to_string())),
        };
        Ok(ToolOutcome {
            result,
            is_error: false,
        })
    }
}

fn pipeline(
    responses: Vec<ChatResponse>,
    registry: Arc<MockRegistry>,
    max_attempts: u32,
) -> (Pipeline, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(responses),
        requests: Mutex::new(Vec::new()),
    });
    let mut providers: std::collections::HashMap<String, Arc<dyn ChatProvider>> =
        std::collections::HashMap::new();
    providers.insert("mock".to_string(), provider.clone());
    let roles = ModelRoles {
        default: Some(ModelChoice {
            provider: "mock".to_string(),
            model: "test".to_string(),
            temperature: None,
            dimensions: None,
        }),
        ..Default::default()
    };
    let router = Arc::new(graph_llm::ModelRouter::with_providers(providers, roles));
    (
        Pipeline {
            router,
            registry,
            events: Arc::new(NullSink),
            store: None,
            user_context: "test user".into(),
            current_date: "2026-07-09".into(),
            max_attempts,
        },
        provider,
    )
}

fn two_step_plan(ref_path: &str) -> Value {
    json!({
        "plan": [
            {"id": "E0", "toolName": "t__search", "input": {"query": "platform"}},
            {"id": "E1", "toolName": "t__issues", "input": {"teamId": format!("{{{{{ref_path}}}}}")}},
        ],
        "solverData": {
            "queryToAnswer": "how is the sprint going",
            "data": {"issues": "{{E1}}"}
        }
    })
}

fn search_registry(values: Value) -> Arc<MockRegistry> {
    Arc::new(MockRegistry {
        search_result: values,
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec![],
    })
}

#[tokio::test]
async fn planned_happy_path_flows_data_between_steps() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, provider) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.id")),
            text("all good"),
        ],
        registry.clone(),
        2,
    );

    let outcome = pipeline.run_planned("sprint status").await.unwrap();
    assert_eq!(outcome.answer, "all good");
    assert!(!outcome.degraded);
    assert_eq!(outcome.state.plan_attempts, 1);

    let invocations = registry.invocations.lock().unwrap();
    assert_eq!(invocations[1].0, "t__issues");
    assert_eq!(
        invocations[1].1,
        json!({"teamId": "team-1"}),
        "typed dataflow"
    );

    // Solver saw the rendered payload.
    let requests = provider.requests.lock().unwrap();
    let solver_request = requests.last().unwrap();
    assert!(solver_request.messages.iter().any(|m| matches!(
        m, graph_llm::types::ChatMessage::User { content } if content.contains("team-1")
    )));
}

#[tokio::test]
async fn bad_path_triggers_replan_with_error_context() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, provider) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.idd")), // typo → BadPath
            structured(two_step_plan("E0.values.0.id")),
            text("recovered"),
        ],
        registry.clone(),
        2,
    );

    let outcome = pipeline.run_planned("sprint status").await.unwrap();
    assert_eq!(outcome.answer, "recovered");
    assert!(!outcome.degraded);
    assert_eq!(outcome.state.plan_attempts, 2);

    // The second planner call carried the BadPath error with the key digest.
    let requests = provider.requests.lock().unwrap();
    assert!(requests[1].system.contains("no key 'idd'"));
    // E0 executed once only — preserved across the replan.
    let invocations = registry.invocations.lock().unwrap();
    let searches = invocations.iter().filter(|(n, _)| n == "t__search").count();
    assert_eq!(searches, 1, "executed steps must not re-run");
}

#[tokio::test]
async fn empty_data_goes_to_solver_without_replanning() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.id")),
            text("nothing found"),
        ],
        registry,
        2,
    );

    let outcome = pipeline.run_planned("sprint status").await.unwrap();
    assert_eq!(outcome.answer, "nothing found");
    assert!(!outcome.degraded);
    assert_eq!(outcome.state.plan_attempts, 1, "EmptyData never replans");

    let requests = provider.requests.lock().unwrap();
    assert!(requests.last().unwrap().system.contains("data ran out"));
}

#[tokio::test]
async fn exhausted_attempts_degrade_to_error_summary() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__search".to_string()],
    });
    let (pipeline, _) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.id")),
            text("sorry, it failed"),
        ],
        registry,
        1,
    );

    let outcome = pipeline.run_planned("sprint status").await.unwrap();
    assert!(outcome.degraded);
    assert_eq!(outcome.answer, "sorry, it failed");
}

#[tokio::test]
async fn explicit_plans_fail_hard_without_replanning() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__search".to_string()],
    });
    let (pipeline, provider) = pipeline(vec![], registry, 3);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}}
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Solve(SolverData::default()), None)
        .await
        .unwrap_err();
    assert!(matches!(err, PipelineError::StepFailed { .. }));
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "no LLM calls on hard failure"
    );
}

#[tokio::test]
async fn explicit_plans_render_input_root() {
    let registry = search_registry(json!({"values": [{"id": "t1"}]}));
    let (pipeline, _) = pipeline(vec![text("done")], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "{{input.team}}"}}
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit(
            "q",
            plan,
            Finish::Solve(SolverData::default()),
            Some(json!({"team": "Platform"})),
        )
        .await
        .unwrap();
    assert_eq!(outcome.answer, "done");
    let invocations = registry.invocations.lock().unwrap();
    assert_eq!(invocations[0].1, json!({"query": "Platform"}));
}

#[tokio::test]
async fn validation_rejects_forward_references() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "{{E1.values}}"}},
        {"id": "E1", "toolName": "t__issues", "input": {}}
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Solve(SolverData::default()), None)
        .await
        .unwrap_err();
    let PipelineError::InvalidPlan(message) = err else {
        panic!("expected InvalidPlan");
    };
    assert!(message.contains("E1"));
}

#[tokio::test]
async fn render_finish_emits_structured_output_without_llm() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, provider) = pipeline(vec![], registry, 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}}
    ]))
    .unwrap();
    let mut output = serde_json::Map::new();
    output.insert("teams".into(), json!("{{E0.values}}"));
    output.insert("count".into(), json!("{{E0.values.length}}"));
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Render(output), None)
        .await
        .unwrap();
    assert_eq!(
        outcome.structured,
        Some(json!({"teams": [{"id": "team-1"}], "count": 1}))
    );
    assert!(provider.requests.lock().unwrap().is_empty(), "no LLM calls");
}

#[tokio::test]
async fn silent_finish_runs_steps_and_produces_nothing() {
    let registry = search_registry(json!({"ok": true}));
    let (pipeline, provider) = pipeline(vec![], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}}
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert!(outcome.answer.is_empty());
    assert!(outcome.structured.is_none());
    assert_eq!(registry.invocations.lock().unwrap().len(), 1, "step ran");
    assert!(provider.requests.lock().unwrap().is_empty(), "no LLM calls");
}

#[tokio::test]
async fn empty_data_is_a_hard_failure_without_a_solver() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(vec![], registry, 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "t__issues", "input": {"teamId": "{{E0.values.0.id}}"}}
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(matches!(err, PipelineError::EmptyData { .. }));
}
