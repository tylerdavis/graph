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
            plans: Arc::new(Vec::new()),
            call_stack: Vec::new(),
            store: None,
            gate: None,
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

/// Minimal in-test Store: only the shape-cache methods matter here.
struct ShapeOnlyStore {
    shapes: Mutex<Vec<crate::store::ToolShape>>,
}

#[async_trait]
impl crate::store::Store for ShapeOnlyStore {
    async fn create_thread(
        &self,
        _: &str,
    ) -> Result<crate::store::ThreadMeta, crate::store::StoreError> {
        unimplemented!()
    }
    async fn get_thread(
        &self,
        _: &str,
    ) -> Result<Option<crate::store::ThreadMeta>, crate::store::StoreError> {
        unimplemented!()
    }
    async fn latest_thread(
        &self,
    ) -> Result<Option<crate::store::ThreadMeta>, crate::store::StoreError> {
        unimplemented!()
    }
    async fn list_threads(
        &self,
    ) -> Result<Vec<crate::store::ThreadMeta>, crate::store::StoreError> {
        unimplemented!()
    }
    async fn delete_thread(&self, _: &str) -> Result<bool, crate::store::StoreError> {
        unimplemented!()
    }
    async fn set_title(&self, _: &str, _: &str) -> Result<(), crate::store::StoreError> {
        unimplemented!()
    }
    async fn append_messages(
        &self,
        _: &str,
        _: &[graph_llm::types::ChatMessage],
    ) -> Result<(), crate::store::StoreError> {
        unimplemented!()
    }
    async fn load_messages(
        &self,
        _: &str,
    ) -> Result<Vec<graph_llm::types::ChatMessage>, crate::store::StoreError> {
        unimplemented!()
    }
    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), crate::store::StoreError> {
        self.shapes.lock().unwrap().push(crate::store::ToolShape {
            tool: tool.to_string(),
            schema: schema.clone(),
            example: example.clone(),
            seen_count: 1,
        });
        Ok(())
    }
    async fn tool_shapes(&self) -> Result<Vec<crate::store::ToolShape>, crate::store::StoreError> {
        Ok(self.shapes.lock().unwrap().clone())
    }
}

#[tokio::test]
async fn shapes_recorded_mid_run_reach_the_next_planning_attempt() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let store = Arc::new(ShapeOnlyStore {
        shapes: Mutex::new(Vec::new()),
    });
    let (mut pipeline, provider) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.idd")), // BadPath → replan
            structured(two_step_plan("E0.values.0.id")),
            text("done"),
        ],
        registry,
        2,
    );
    pipeline.store = Some(store.clone());

    // The pipeline was constructed BEFORE this shape is recorded — under the
    // old construction-time snapshot, no planner prompt would ever see it.
    store
        .record_tool_shape(
            "t__search",
            &json!({"type": "object"}),
            &json!({"values": [{"id": "team-1"}]}),
        )
        .await
        .unwrap();

    pipeline.run_planned("q").await.unwrap();
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[0].system.contains("observedOutputShape"),
        "planner prompts must read the shape cache at plan time, not construction time"
    );
    assert!(requests[1].system.contains("observedOutputShape"));
}

fn exit_plan(when_value: &str, status: &str) -> Plan {
    serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "exit", "input": {
            "when": {"value": when_value, "op": "eq", "to": 0},
            "status": status,
            "message": "gate fired",
        }},
        {"id": "E2", "toolName": "t__issues", "input": {"q": "{{E0.values}}"}}
    ]))
    .unwrap()
}

#[tokio::test]
async fn exit_success_skips_remaining_steps_and_solver() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(vec![], registry.clone(), 1);
    let outcome = pipeline
        .run_explicit(
            "q",
            exit_plan("{{E0.values.length}}", "success"),
            Finish::Solve(SolverData::default()),
            None,
        )
        .await
        .unwrap();
    let exit = outcome.exit.expect("exited");
    assert_eq!(exit.status, crate::pipeline::ExitStatus::Success);
    assert_eq!(outcome.answer, "gate fired");
    // E2 never ran; solver never called.
    assert_eq!(registry.invocations.lock().unwrap().len(), 1);
    assert!(provider.requests.lock().unwrap().is_empty(), "no LLM calls");
}

#[tokio::test]
async fn exit_gate_passes_and_plan_continues() {
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (pipeline, _) = pipeline(vec![text("done")], registry.clone(), 1);
    let outcome = pipeline
        .run_explicit(
            "q",
            exit_plan("{{E0.values.length}}", "success"),
            Finish::Solve(SolverData::default()),
            None,
        )
        .await
        .unwrap();
    assert!(outcome.exit.is_none());
    assert_eq!(outcome.answer, "done");
    // Gate result is referenceable.
    assert_eq!(outcome.state.results["E1"]["passed"], json!(true));
    assert_eq!(
        registry.invocations.lock().unwrap().len(),
        2,
        "E0 and E2 ran"
    );
}

#[tokio::test]
async fn inferred_exit_uses_judge_verdict() {
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (pipeline, provider) = pipeline(
        vec![structured(
            json!({"verdict": true, "reason": "clearly blocked"}),
        )],
        registry,
        1,
    );
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "exit", "input": {
            "infer": "Is this blocked? {{E0.values}}",
            "status": "error",
            "message": "Blocked",
        }}
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let exit = outcome.exit.expect("exited");
    assert_eq!(exit.status, crate::pipeline::ExitStatus::Error);
    assert_eq!(exit.message, "Blocked (clearly blocked)");
    assert_eq!(exit.reason.as_deref(), Some("clearly blocked"));
    // The verdict question included the rendered data.
    let requests = provider.requests.lock().unwrap();
    assert!(matches!(
        &requests[0].messages[0],
        graph_llm::types::ChatMessage::User { content } if content.contains("\"id\"")
    ));
}

#[tokio::test]
async fn planner_gets_the_exit_tool_and_authored_exits_work() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![structured(json!({
            "plan": [
                {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
                {"id": "E1", "toolName": "exit", "input": {
                    "when": {"value": "{{E0.values.length}}", "op": "eq", "to": 0},
                    "status": "success",
                    "message": "nothing to do",
                }}
            ],
            "solverData": {"queryToAnswer": "q", "data": {}}
        }))],
        registry,
        1,
    );
    let outcome = pipeline.run_planned("find work").await.unwrap();
    assert_eq!(outcome.exit.expect("exited").message, "nothing to do");
    // The planner prompt described the exit tool.
    let requests = provider.requests.lock().unwrap();
    assert!(requests[0].system.contains("\"name\":\"exit\""));
}

/// E0 searches, E1 decides on `{{E0.values.length}} gt 0`.
fn decide_plan(then: Value, else_branch: Option<Value>) -> Plan {
    let mut input = json!({
        "if": {"value": "{{E0.values.length}}", "op": "gt", "to": 0},
        "then": then,
    });
    if let Some(else_branch) = else_branch {
        input["else"] = else_branch;
    }
    serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": input},
    ]))
    .unwrap()
}

#[tokio::test]
async fn decide_then_branch_runs_single_call() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, provider) = pipeline(vec![], registry.clone(), 1);
    let plan = decide_plan(
        json!({"toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}}),
        Some(json!({"toolName": "t__search", "input": {"query": "fallback"}})),
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let decision = &outcome.state.results["E1"];
    assert_eq!(decision["branch"], json!("then"));
    assert_eq!(decision["verdict"], json!(true));
    assert_eq!(decision["reason"], json!(null));
    assert_eq!(
        decision["result"],
        json!({"got": {"q": "team-1"}}),
        "typed dataflow into the branch"
    );

    let invocations = registry.invocations.lock().unwrap();
    let searches = invocations.iter().filter(|(n, _)| n == "t__search").count();
    assert_eq!(searches, 1, "else branch never invoked");
    assert_eq!(
        outcome.state.steps_executed(),
        3,
        "E0 + decide + 1 branch call"
    );
    assert!(provider.requests.lock().unwrap().is_empty(), "no LLM calls");
}

#[tokio::test]
async fn decide_else_branch_runs_and_poisoned_then_is_never_rendered() {
    // E0 finds nothing; `then` indexes into the empty array (EmptyData if
    // rendered) — the exact case `else` exists to handle.
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let plan = decide_plan(
        json!({"toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}}),
        Some(json!({"toolName": "t__issues", "input": {"q": "none"}})),
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let decision = &outcome.state.results["E1"];
    assert_eq!(decision["branch"], json!("else"));
    assert_eq!(decision["verdict"], json!(false));
    assert_eq!(decision["result"], json!({"got": {"q": "none"}}));
}

#[tokio::test]
async fn decide_poisoned_else_is_never_rendered() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan = decide_plan(
        json!({"toolName": "t__issues", "input": {"q": "safe"}}),
        Some(json!({"toolName": "t__issues", "input": {"q": "{{E0.nope.deep}}"}})),
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E1"]["branch"], json!("then"));
}

#[tokio::test]
async fn decide_without_else_passes_through() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let mut plan = decide_plan(
        json!({"toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}}),
        None,
    );
    plan.push(
        serde_json::from_value(json!(
            {"id": "E2", "toolName": "t__issues", "input": {"q": "{{E1.verdict}}"}}
        ))
        .unwrap(),
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let decision = &outcome.state.results["E1"];
    assert_eq!(decision["branch"], json!(null));
    assert_eq!(decision["verdict"], json!(false));
    assert_eq!(decision["result"], json!(null));
    // E2 still ran — the plan continued past the decide.
    assert_eq!(outcome.state.results["E2"], json!({"got": {"q": false}}));
}

#[tokio::test]
async fn inferred_decide_uses_judge_verdict() {
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (pipeline, provider) = pipeline(
        vec![structured(json!({"verdict": true, "reason": "urgent"}))],
        registry,
        1,
    );
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": {
            "infer": "Is this urgent? {{E0.values}}",
            "then": {"toolName": "t__issues", "input": {"q": "escalate"}},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let decision = &outcome.state.results["E1"];
    assert_eq!(decision["branch"], json!("then"));
    assert_eq!(decision["reason"], json!("urgent"));
    // The verdict question included the rendered data.
    let requests = provider.requests.lock().unwrap();
    assert!(matches!(
        &requests[0].messages[0],
        graph_llm::types::ChatMessage::User { content } if content.contains("\"id\"")
    ));
}

#[tokio::test]
async fn inline_branch_steps_flow_data_and_stay_scoped() {
    let registry = search_registry(json!({"values": [{"id": "x1"}]}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let plan = decide_plan(
        json!([
            {"id": "E10", "toolName": "t__search", "input": {"query": "{{E0.values.0.id}}"}},
            {"id": "E11", "toolName": "t__issues", "input": {"q": "{{E10.values.0.id}}"}},
        ]),
        None,
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let decision = &outcome.state.results["E1"];
    assert_eq!(
        decision["result"],
        json!({"got": {"q": "x1"}}),
        "intra-branch dataflow, last step wins"
    );
    assert!(
        !outcome.state.results.contains_key("E10"),
        "branch ids stay scoped"
    );
    assert!(!outcome.state.results.contains_key("E11"));
    assert_eq!(
        outcome.state.steps_executed(),
        4,
        "E0 + decide + 2 branch steps"
    );
    // The branch's inner search received the outer step's data.
    let invocations = registry.invocations.lock().unwrap();
    assert_eq!(invocations[1].1, json!({"query": "x1"}));
}

#[tokio::test]
async fn decide_branch_calling_plan_surfaces_nested_exit() {
    let inner = plan_doc_yaml(
        r#"
identifier: inner
name: Inner
description: asserts
steps:
  - id: E0
    tool_name: exit
    input: { status: error, message: "inner assertion" }
"#,
    );
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![inner]);
    let plan = decide_plan(json!({"toolName": "plan__inner", "input": {}}), None);
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::StepFailed {
        step,
        tool,
        message,
    } = err
    else {
        panic!("expected StepFailed");
    };
    assert_eq!(step, "E1");
    assert_eq!(tool, "decide");
    assert!(message.contains("inner assertion"), "{message}");
}

#[tokio::test]
async fn branch_failure_fails_the_decide_step_and_replans_in_planned_mode() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({"values": [{"id": 1}]}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__issues".to_string()],
    });
    // Explicit plans: hard failure attributed to the decide step.
    let (pipeline_explicit, _) = pipeline(vec![], registry.clone(), 1);
    let plan = decide_plan(json!({"toolName": "t__issues", "input": {"q": "x"}}), None);
    let err = pipeline_explicit
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::StepFailed {
        step,
        tool,
        message,
    } = err
    else {
        panic!("expected StepFailed");
    };
    assert_eq!((step.as_str(), tool.as_str()), ("E1", "decide"));
    assert!(message.contains("`then` branch"), "{message}");

    // Planned mode: the failure lands on the bus and triggers a replan.
    let decide_step = json!({"id": "E1", "toolName": "decide", "input": {
        "if": {"value": "{{E0.values.length}}", "op": "gt", "to": 0},
        "then": {"toolName": "t__issues", "input": {"q": "x"}},
    }});
    let (pipeline_planned, provider) = pipeline(
        vec![
            structured(json!({
                "plan": [
                    {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
                    decide_step,
                ],
                "solverData": {"queryToAnswer": "q", "data": {}}
            })),
            structured(json!({
                "plan": [{"id": "E1", "toolName": "t__search", "input": {"query": "retry"}}],
                "solverData": {"queryToAnswer": "q", "data": {}}
            })),
            text("recovered"),
        ],
        registry,
        2,
    );
    let outcome = pipeline_planned.run_planned("q").await.unwrap();
    assert_eq!(outcome.answer, "recovered");
    assert_eq!(outcome.state.plan_attempts, 2);
    // The replanning prompt carried the branch failure.
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[1].system.contains("`then` branch"),
        "error context reaches the planner"
    );
}

#[tokio::test]
async fn empty_data_in_chosen_branch_degrades_normally() {
    // E0 returns an empty list and the gate sends us into a branch whose
    // template needs an element: genuine EmptyData, not a plan defect.
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": {
            "if": {"value": "{{E0.values.length}}", "op": "eq", "to": 0},
            "then": {"toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}},
        }},
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(matches!(err, PipelineError::EmptyData { .. }));
}

#[tokio::test]
async fn decide_validation_rejections() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);

    let run = |input: Value| {
        let plan: Plan = serde_json::from_value(json!([
            {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
            {"id": "E1", "toolName": "decide", "input": input},
        ]))
        .unwrap();
        let pipeline = pipeline.clone();
        async move {
            let err = pipeline
                .run_explicit("q", plan, Finish::Silent, None)
                .await
                .unwrap_err();
            let PipelineError::InvalidPlan(message) = err else {
                panic!("expected InvalidPlan");
            };
            message
        }
    };
    let call = json!({"toolName": "t__issues", "input": {}});

    let message = run(json!({
        "if": {"value": 1, "op": "eq", "to": 1}, "infer": "both?", "then": call,
    }))
    .await;
    assert!(message.contains("mutually exclusive"), "{message}");

    let message = run(json!({"then": call})).await;
    assert!(message.contains("`if` or `infer`"), "{message}");

    let message = run(json!({
        "if": {"value": 1, "op": "eq", "to": 1},
        "then": {"toolName": "decide", "input": {}},
    }))
    .await;
    assert!(message.contains("cannot nest"), "{message}");

    // Cross-branch reference: else reads a then-branch id.
    let message = run(json!({
        "if": {"value": 1, "op": "eq", "to": 1},
        "then": [{"id": "E10", "toolName": "t__search", "input": {"query": "x"}}],
        "else": [{"id": "E11", "toolName": "t__issues", "input": {"q": "{{E10.values}}"}}],
    }))
    .await;
    assert!(message.contains("E10"), "{message}");

    // Forward reference within a branch.
    let message = run(json!({
        "if": {"value": 1, "op": "eq", "to": 1},
        "then": [
            {"id": "E10", "toolName": "t__search", "input": {"query": "{{E11.values}}"}},
            {"id": "E11", "toolName": "t__issues", "input": {"q": "y"}},
        ],
    }))
    .await;
    assert!(message.contains("E11"), "{message}");

    // Branch step id shadowing a top-level id.
    let message = run(json!({
        "if": {"value": 1, "op": "eq", "to": 1},
        "then": [{"id": "E0", "toolName": "t__search", "input": {"query": "x"}}],
    }))
    .await;
    assert!(message.contains("collides"), "{message}");
}

#[tokio::test]
async fn planner_gets_the_decide_tool_and_authored_decides_work() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, provider) = pipeline(
        vec![
            structured(json!({
                "plan": [
                    {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
                    {"id": "E1", "toolName": "decide", "input": {
                        "if": {"value": "{{E0.values.length}}", "op": "gt", "to": 0},
                        "then": {"toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}},
                    }},
                ],
                "solverData": {"queryToAnswer": "q", "data": {"taken": "{{E1.branch}}"}}
            })),
            text("done"),
        ],
        registry,
        1,
    );
    let outcome = pipeline.run_planned("route it").await.unwrap();
    assert_eq!(outcome.answer, "done");
    assert_eq!(outcome.state.results["E1"]["branch"], json!("then"));
    let requests = provider.requests.lock().unwrap();
    assert!(requests[0].system.contains("\"name\":\"decide\""));
}

#[tokio::test]
async fn decide_yaml_doc_round_trips_and_runs() {
    let fork = plan_doc_yaml(
        r#"
identifier: fork
name: Fork
description: forks on search results
steps:
  - id: E0
    tool_name: t__search
    input: { query: "x" }
  - id: E1
    tool_name: decide
    input:
      if: { value: "{{E0.values.length}}", op: gt, to: 0 }
      then:
        tool_name: t__issues
        input: { q: "{{E0.values.0.id}}" }
      else:
        - id: E10
          tool_name: t__search
          input: { query: "fallback" }
        - id: E11
          tool_name: t__issues
          input: { q: "{{E10.values}}" }
output:
  taken: "{{E1.branch}}"
  result: "{{E1.result}}"
"#,
    );
    let registry = search_registry(json!({"values": [{"id": "z9"}]}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![fork]);
    let call = pipeline.call_plan("fork", json!({})).await;
    assert!(!call.is_error, "{:?}", call.result);
    assert_eq!(call.result["taken"], json!("then"));
    assert_eq!(call.result["result"], json!({"got": {"q": "z9"}}));
}

#[test]
fn decide_doc_rejects_exit_in_branch() {
    let doc: crate::pipeline::doc::PlanDoc = serde_yaml::from_str(
        r#"
identifier: bad
name: Bad
description: exit nested in branch
steps:
  - id: E0
    tool_name: decide
    input:
      if: { value: 1, op: eq, to: 1 }
      then:
        tool_name: exit
        input: { status: success }
"#,
    )
    .unwrap();
    let err = crate::pipeline::doc::validate_doc(&doc).unwrap_err();
    assert!(err.contains("cannot nest"), "{err}");
}

#[tokio::test]
async fn map_single_call_runs_per_item_with_ordered_results() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}));
    let (pipeline, provider) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"q": "{{item.id}}", "n": "{{index}}"}},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let map = &outcome.state.results["E1"];
    assert_eq!(map["count"], json!(3));
    assert_eq!(
        map["results"],
        json!([
            {"got": {"q": "a", "n": 0}},
            {"got": {"q": "b", "n": 1}},
            {"got": {"q": "c", "n": 2}},
        ]),
        "typed per-item dataflow, input order"
    );
    assert_eq!(outcome.state.steps_executed(), 5, "E0 + map + 3 item calls");
    assert!(provider.requests.lock().unwrap().is_empty(), "no LLM calls");
}

#[tokio::test]
async fn map_inline_steps_flow_data_and_stay_scoped() {
    let registry = search_registry(json!({"values": [{"id": "x1"}]}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": [
                {"id": "E10", "toolName": "t__search", "input": {"query": "{{item.id}}"}},
                {"id": "E11", "toolName": "t__issues", "input": {"q": "{{E10.values.0.id}}"}},
            ],
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let map = &outcome.state.results["E1"];
    assert_eq!(
        map["results"],
        json!([{"got": {"q": "x1"}}]),
        "intra-body dataflow, last step wins"
    );
    assert!(
        !outcome.state.results.contains_key("E10"),
        "body ids stay scoped"
    );
    assert!(!outcome.state.results.contains_key("E11"));
    assert_eq!(outcome.state.steps_executed(), 4, "E0 + map + 2 body steps");
    // The body's inner search received the item's data.
    let invocations = registry.invocations.lock().unwrap();
    assert_eq!(invocations[1].1, json!({"query": "x1"}));
}

#[tokio::test]
async fn concurrent_map_completes_all_items_in_order() {
    let values: Vec<Value> = (0..5).map(|n| json!({"id": format!("v{n}")})).collect();
    let registry = search_registry(json!({"values": values}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "concurrency": 3,
            "do": {"toolName": "t__issues", "input": {"q": "{{item.id}}"}},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let map = &outcome.state.results["E1"];
    assert_eq!(map["count"], json!(5));
    let expected: Vec<Value> = (0..5)
        .map(|n| json!({"got": {"q": format!("v{n}")}}))
        .collect();
    assert_eq!(
        map["results"],
        json!(expected),
        "input order regardless of concurrency"
    );
    let issues = registry
        .invocations
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == "t__issues")
        .count();
    assert_eq!(issues, 5, "every item ran");
}

#[tokio::test]
async fn map_item_failure_fails_the_step_with_index_attribution() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({"values": [{"id": "a"}, {"id": "b"}]}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__issues".to_string()],
    });
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"q": "{{item.id}}"}},
        }},
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::StepFailed {
        step,
        tool,
        message,
    } = err
    else {
        panic!("expected StepFailed");
    };
    assert_eq!((step.as_str(), tool.as_str()), ("E1", "map"));
    assert!(message.contains("`do` item 0 (t__issues)"), "{message}");
    // The failure halted the iteration — item 1 never started.
    let issues = registry
        .invocations
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == "t__issues")
        .count();
    assert_eq!(issues, 1, "remaining items skipped after the failure");
}

#[tokio::test]
async fn empty_over_continues_with_zero_count() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"q": "{{item.id}}"}},
        }},
        {"id": "E2", "toolName": "t__issues", "input": {"q": "{{E1.count}}"}},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(
        outcome.state.results["E1"],
        json!({"count": 0, "results": []})
    );
    // The plan continued past the empty map.
    assert_eq!(outcome.state.results["E2"], json!({"got": {"q": 0}}));
}

#[tokio::test]
async fn non_array_over_is_a_plan_defect() {
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0}}",
            "do": {"toolName": "t__issues", "input": {"q": "y"}},
        }},
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::StepFailed { message, .. } = err else {
        panic!("expected StepFailed");
    };
    assert!(message.contains("must produce an array"), "{message}");
}

#[tokio::test]
async fn empty_data_in_item_body_degrades_normally() {
    // The item exists but its inner list is empty: genuine EmptyData
    // inside the body, not a plan defect.
    let registry = search_registry(json!({"values": [{"children": []}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"q": "{{item.children.0.id}}"}},
        }},
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(matches!(err, PipelineError::EmptyData { .. }));
}

#[tokio::test]
async fn map_body_calling_plan_surfaces_nested_exit() {
    let inner = plan_doc_yaml(
        r#"
identifier: inner
name: Inner
description: asserts
steps:
  - id: E0
    tool_name: exit
    input: { status: error, message: "inner assertion" }
"#,
    );
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![inner]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "plan__inner", "input": {}},
        }},
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::StepFailed {
        step,
        tool,
        message,
    } = err
    else {
        panic!("expected StepFailed");
    };
    assert_eq!((step.as_str(), tool.as_str()), ("E1", "map"));
    assert!(message.contains("inner assertion"), "{message}");
}

#[tokio::test]
async fn reduce_folds_left_threading_the_accumulator() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "reduce", "input": {
            "over": "{{E0.values}}",
            "initial": {"seen": "none"},
            "do": {"toolName": "t__issues", "input": {"a": "{{accumulator}}", "i": "{{item.id}}"}},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let reduce = &outcome.state.results["E1"];
    assert_eq!(reduce["count"], json!(2));
    // Second iteration saw the first's result as its accumulator.
    assert_eq!(reduce["result"]["got"]["i"], json!("b"));
    assert_eq!(reduce["result"]["got"]["a"]["got"]["i"], json!("a"));
    assert_eq!(
        reduce["result"]["got"]["a"]["got"]["a"],
        json!({"seen": "none"}),
        "first iteration started from `initial`"
    );
    assert_eq!(
        outcome.state.steps_executed(),
        4,
        "E0 + reduce + 2 item calls"
    );
}

#[tokio::test]
async fn reduce_defaults_initial_to_null_and_empty_over_returns_it() {
    let registry = search_registry(json!({"values": [{"id": "a"}]}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "reduce", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"a": "{{accumulator}}"}},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(
        outcome.state.results["E1"]["result"]["got"]["a"],
        json!(null),
        "accumulator starts at null when initial is omitted"
    );

    // Empty list: the result is the initial value untouched.
    let empty_registry = search_registry(json!({"values": []}));
    let (empty_pipeline, _) = super::tests::pipeline(vec![], empty_registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "reduce", "input": {
            "over": "{{E0.values}}",
            "initial": {"total": 0},
            "do": {"toolName": "t__issues", "input": {"a": "{{accumulator}}"}},
        }},
    ]))
    .unwrap();
    let outcome = empty_pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(
        outcome.state.results["E1"],
        json!({"count": 0, "result": {"total": 0}})
    );
}

#[tokio::test]
async fn iteration_validation_rejections() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);

    let run = |steps: Value| {
        let plan: Plan = serde_json::from_value(steps).unwrap();
        let pipeline = pipeline.clone();
        async move {
            let err = pipeline
                .run_explicit("q", plan, Finish::Silent, None)
                .await
                .unwrap_err();
            let PipelineError::InvalidPlan(message) = err else {
                panic!("expected InvalidPlan");
            };
            message
        }
    };

    // Reduce has no concurrency knob — each iteration reads the previous
    // accumulator.
    let message = run(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "reduce", "input": {
            "over": "{{E0.values}}",
            "concurrency": 2,
            "do": {"toolName": "t__issues", "input": {"q": "y"}},
        }},
    ]))
    .await;
    assert!(message.contains("concurrency"), "{message}");

    // Control steps cannot nest: map inside a decide branch…
    let message = run(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": {
            "if": {"value": 1, "op": "eq", "to": 1},
            "then": {"toolName": "map", "input": {}},
        }},
    ]))
    .await;
    assert!(message.contains("cannot nest"), "{message}");

    // …and decide inside a map body.
    let message = run(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "decide", "input": {}},
        }},
    ]))
    .await;
    assert!(message.contains("cannot nest"), "{message}");

    // Pseudo-roots outside their scope.
    let message = run(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"a": "{{accumulator}}"}},
        }},
    ]))
    .await;
    assert!(message.contains("reduce body"), "{message}");
}

#[tokio::test]
async fn planner_gets_the_iteration_tools_and_authored_maps_work() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (pipeline, provider) = pipeline(
        vec![
            structured(json!({
                "plan": [
                    {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
                    {"id": "E1", "toolName": "map", "input": {
                        "over": "{{E0.values}}",
                        "do": {"toolName": "t__issues", "input": {"q": "{{item.id}}"}},
                    }},
                ],
                "solverData": {"queryToAnswer": "q", "data": {"mapped": "{{E1.results}}"}}
            })),
            text("done"),
        ],
        registry,
        1,
    );
    let outcome = pipeline.run_planned("fan out").await.unwrap();
    assert_eq!(outcome.answer, "done");
    assert_eq!(outcome.state.results["E1"]["count"], json!(2));
    let requests = provider.requests.lock().unwrap();
    assert!(requests[0].system.contains("\"name\":\"map\""));
    assert!(requests[0].system.contains("\"name\":\"reduce\""));
}

#[tokio::test]
async fn iteration_yaml_doc_round_trips_and_runs() {
    let fanout = plan_doc_yaml(
        r#"
identifier: fanout
name: Fanout
description: maps then folds
steps:
  - id: E0
    tool_name: t__search
    input: { query: "x" }
  - id: E1
    tool_name: map
    input:
      over: "{{E0.values}}"
      concurrency: 2
      do:
        tool_name: t__issues
        input: { q: "{{item.id}}" }
  - id: E2
    tool_name: reduce
    input:
      over: "{{E1.results}}"
      initial: { first: null }
      do:
        tool_name: t__issues
        input: { accumulator: "{{accumulator}}", item: "{{item}}" }
output:
  mapped: "{{E1.results}}"
  folded: "{{E2.result}}"
"#,
    );
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![fanout]);
    let call = pipeline.call_plan("fanout", json!({})).await;
    assert!(!call.is_error, "{:?}", call.result);
    assert_eq!(
        call.result["mapped"],
        json!([{"got": {"q": "a"}}, {"got": {"q": "b"}}])
    );
    assert_eq!(
        call.result["folded"]["got"]["item"],
        json!({"got": {"q": "b"}}),
        "reduce folded the map's results"
    );
}

fn plan_doc_yaml(yaml: &str) -> crate::pipeline::doc::PlanDoc {
    let doc: crate::pipeline::doc::PlanDoc = serde_yaml::from_str(yaml).unwrap();
    crate::pipeline::doc::validate_doc(&doc).unwrap();
    doc
}

#[tokio::test]
async fn plans_call_plans_with_dataflow() {
    let inner = plan_doc_yaml(
        r#"
identifier: inner
name: Inner
description: fetch and shape
steps:
  - id: E0
    tool_name: t__search
    input: { query: "{{input.q}}" }
output:
  found: "{{E0.values}}"
"#,
    );
    let outer = plan_doc_yaml(
        r#"
identifier: outer
name: Outer
description: composes inner
steps:
  - id: E0
    tool_name: plan__inner
    input: { q: "hello" }
output:
  inner_found: "{{E0.found}}"
"#,
    );
    let registry = search_registry(json!({"values": [{"id": "x"}]}));
    let (mut pipeline, _) = pipeline(vec![], registry.clone(), 1);
    pipeline.plans = Arc::new(vec![inner, outer]);

    let call = pipeline.call_plan("outer", json!({})).await;
    assert!(!call.is_error, "{:?}", call.result);
    assert_eq!(call.result, json!({"inner_found": [{"id": "x"}]}));
    // inner's step actually ran against the base registry
    assert_eq!(
        registry.invocations.lock().unwrap()[0].1,
        json!({"query": "hello"})
    );
}

#[tokio::test]
async fn plan_cycles_error_cleanly() {
    let a = plan_doc_yaml(
        r#"
identifier: a
name: A
description: calls b
steps:
  - { id: E0, tool_name: plan__b, input: {} }
"#,
    );
    let b = plan_doc_yaml(
        r#"
identifier: b
name: B
description: calls a
steps:
  - { id: E0, tool_name: plan__a, input: {} }
"#,
    );
    let registry = search_registry(json!({}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![a, b]);

    let call = pipeline.call_plan("a", json!({})).await;
    assert!(call.is_error);
    let message = call.result.to_string();
    assert!(message.contains("cycle"), "{message}");
    assert!(message.contains("a → b"), "{message}");
}

#[tokio::test]
async fn exit_inside_nested_plan_surfaces_to_the_caller() {
    let inner = plan_doc_yaml(
        r#"
identifier: inner
name: Inner
description: asserts
steps:
  - id: E0
    tool_name: exit
    input: { status: error, message: "inner assertion" }
"#,
    );
    let outer = plan_doc_yaml(
        r#"
identifier: outer
name: Outer
description: composes
steps:
  - { id: E0, tool_name: plan__inner, input: {} }
"#,
    );
    let registry = search_registry(json!({}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![inner, outer]);

    // The nested error-exit becomes a failed step in the outer plan —
    // explicit outer plan → hard failure naming the inner assertion.
    let call = pipeline.call_plan("outer", json!({})).await;
    assert!(call.is_error);
    assert!(call.result.to_string().contains("inner assertion"));
}

// ── Execution gate, draft planning, step events ──────────────────────────

/// Gate that consumes scripted decisions in order (exhausted = Proceed)
/// and records every consultation.
struct ScriptedGate {
    decisions: Mutex<Vec<GateDecision>>,
    /// (call_stack, path, tool) per consultation, in order.
    seen: Mutex<Vec<(Vec<String>, String, String)>>,
}

impl ScriptedGate {
    fn new(decisions: Vec<GateDecision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect()
    }

    fn tools(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, tool)| tool.clone())
            .collect()
    }
}

#[async_trait]
impl ExecutionGate for ScriptedGate {
    async fn before_tool(&self, ctx: GateContext<'_>) -> GateDecision {
        self.seen.lock().unwrap().push((
            ctx.call_stack.to_vec(),
            ctx.path.to_string(),
            ctx.tool_name.to_string(),
        ));
        let mut decisions = self.decisions.lock().unwrap();
        if decisions.is_empty() {
            GateDecision::Proceed
        } else {
            decisions.remove(0)
        }
    }
}

/// Sink capturing step_finished events: (path, tool, result, is_error).
#[derive(Default)]
struct RecordingSink {
    finished: Mutex<Vec<(String, String, Value, bool)>>,
}

impl crate::EventSink for RecordingSink {
    fn step_finished(
        &self,
        _call_stack: &[String],
        path: &str,
        tool: &str,
        result: &Value,
        is_error: bool,
        _elapsed: std::time::Duration,
    ) {
        self.finished.lock().unwrap().push((
            path.to_string(),
            tool.to_string(),
            result.clone(),
            is_error,
        ));
    }
}

#[tokio::test]
async fn draft_plan_returns_output_without_executing() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![structured(two_step_plan("E0.values.0.id"))],
        registry.clone(),
        1,
    );
    let output = pipeline
        .draft_plan("sprint status", None, None)
        .await
        .unwrap();
    assert_eq!(output.plan.len(), 2);
    assert_eq!(output.plan[0].id, "E0");
    assert_eq!(
        output.solver_data.query_to_answer,
        "how is the sprint going"
    );
    assert!(
        registry.invocations.lock().unwrap().is_empty(),
        "draft must not execute"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one planner call, no solver");
    assert!(!requests[0].system.contains("Draft Under Revision"));
}

#[tokio::test]
async fn draft_plan_revision_carries_draft_and_error() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![structured(two_step_plan("E0.values.0.id"))],
        registry,
        1,
    );
    let existing: PlannerOutput = serde_json::from_value(two_step_plan("E0.values.0.id")).unwrap();
    pipeline
        .draft_plan(
            "also fetch comments",
            Some(&existing),
            Some("E1 references E9, which is not an earlier step"),
        )
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let system = &requests[0].system;
    assert!(system.contains("Draft Under Revision"), "revision section");
    assert!(system.contains("t__search"), "serialized draft in prompt");
    assert!(system.contains("E1 references E9"), "last error in prompt");
    assert!(
        system.contains("<existing_plan>\n(none)\n</existing_plan>"),
        "the draft must not occupy the executed-steps slot"
    );
}

#[tokio::test]
async fn gate_proceed_is_transparent() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, _) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.id")),
            text("all good"),
        ],
        registry,
        2,
    );
    let gate = ScriptedGate::new(vec![]);
    let outcome = pipeline
        .with_gate(gate.clone())
        .run_planned("sprint status")
        .await
        .unwrap();
    assert_eq!(outcome.answer, "all good");
    assert_eq!(gate.paths(), vec!["E0", "E1"]);
}

#[tokio::test]
async fn gate_skip_injects_result_downstream() {
    let registry = search_registry(json!({"values": [{"id": "real"}]}));
    let (mut pipeline, _) = pipeline(
        vec![structured(two_step_plan("E0.values.0.id")), text("done")],
        registry.clone(),
        2,
    );
    let sink = Arc::new(RecordingSink::default());
    pipeline.events = sink.clone();
    let injected = json!({"values": [{"id": "fake"}]});
    let gate = ScriptedGate::new(vec![GateDecision::Skip {
        result: injected.clone(),
    }]);
    let outcome = pipeline
        .with_gate(gate)
        .run_planned("sprint status")
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E0"], injected);

    let invocations = registry.invocations.lock().unwrap();
    assert_eq!(invocations.len(), 1, "t__search was skipped");
    assert_eq!(invocations[0].0, "t__issues");
    assert_eq!(
        invocations[0].1,
        json!({"teamId": "fake"}),
        "downstream template consumed the injected value"
    );

    let finished = sink.finished.lock().unwrap();
    let e0 = finished.iter().find(|(path, ..)| path == "E0").unwrap();
    assert_eq!(e0.2, injected, "skip still emits a step_finished");
    assert!(!e0.3);
}

#[tokio::test]
async fn gate_abort_is_hard_and_never_replans() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, provider) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.id")),
            structured(two_step_plan("E0.values.0.id")),
            text("never"),
        ],
        registry.clone(),
        2,
    );
    let gate = ScriptedGate::new(vec![GateDecision::Abort]);
    let err = pipeline
        .with_gate(gate)
        .run_planned("sprint status")
        .await
        .unwrap_err();
    let PipelineError::Aborted { step, state } = err else {
        panic!("expected Aborted, got {err}");
    };
    assert_eq!(step, "E0");
    assert_eq!(state.plan.len(), 2, "partial state carries the plan");
    assert!(registry.invocations.lock().unwrap().is_empty());
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1,
        "planner only — no replan, no error summary"
    );
}

#[tokio::test]
async fn gate_fires_inside_decide_branch_and_map_body() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let gate = ScriptedGate::new(vec![]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": {
            "if": {"value": "{{E0.values.length}}", "op": "gt", "to": 0},
            "then": {"toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}},
        }},
        {"id": "E2", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": [{"id": "E10", "toolName": "t__issues", "input": {"q": "{{item.id}}"}}],
        }},
    ]))
    .unwrap();
    pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(
        gate.paths(),
        vec!["E0", "E1/then", "E2/do.0/E10", "E2/do.1/E10"]
    );
    assert!(
        gate.tools().iter().all(|t| t != "decide" && t != "map"),
        "control steps are never gated"
    );
}

#[tokio::test]
async fn gate_abort_in_map_skips_remaining_items() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}, {"id": "c"}]}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let gate = ScriptedGate::new(vec![GateDecision::Proceed, GateDecision::Abort]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"q": "{{item.id}}"}},
        }},
    ]))
    .unwrap();
    let err = pipeline
        .with_gate(gate)
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::Aborted { step, .. } = err else {
        panic!("expected Aborted, got {err}");
    };
    assert_eq!(step, "E1");
    let invocations = registry.invocations.lock().unwrap();
    assert!(
        invocations.iter().all(|(name, _)| name != "t__issues"),
        "aborted item never ran; remaining items were skipped"
    );
}

#[tokio::test]
async fn gate_abort_inside_nested_plan_propagates() {
    let inner = plan_doc_yaml(
        r#"
identifier: inner
name: Inner
description: inner plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: inner }
"#,
    );
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![inner]);
    let gate = ScriptedGate::new(vec![GateDecision::Proceed, GateDecision::Abort]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "plan__inner", "input": {}},
    ]))
    .unwrap();
    let err = pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::Aborted { step, .. } = err else {
        panic!("expected Aborted (not a replannable tool error), got {err}");
    };
    assert_eq!(step, "E0");
    let seen = gate.seen.lock().unwrap();
    assert!(seen[0].0.is_empty(), "outer call has an empty call stack");
    assert_eq!(
        seen[1].0,
        vec!["inner".to_string()],
        "inner call carries the plan frame"
    );
}

#[tokio::test]
async fn step_events_attribute_body_and_control_results() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    let sink = Arc::new(RecordingSink::default());
    pipeline.events = sink.clone();
    let plan = decide_plan(
        json!([
            {"id": "E10", "toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}},
        ]),
        None,
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert!(
        !outcome.state.results.contains_key("E10"),
        "body results stay scoped"
    );
    let finished = sink.finished.lock().unwrap();
    let body = finished
        .iter()
        .find(|(path, ..)| path == "E1/then/E10")
        .expect("body step event with a scoped path");
    assert_eq!(body.2, json!({"got": {"q": "team-1"}}));
    let decide = finished
        .iter()
        .find(|(path, tool, ..)| path == "E1" && tool == "decide")
        .expect("decide aggregate event");
    assert_eq!(decide.2["branch"], json!("then"));
}

#[tokio::test]
async fn validate_plan_reports_all_problems() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "{{E5.values}}"}},
        {"id": "E1", "toolName": "decide", "input": {"then": {"toolName": "t__issues", "input": {}}}},
    ]))
    .unwrap();
    let problems = pipeline.validate_plan(&plan).unwrap_err();
    assert!(problems.iter().any(|p| p.contains("E5")), "{problems:?}");
    assert!(
        problems.iter().any(|p| p.contains("`if` or `infer`")),
        "{problems:?}"
    );
}
