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
        thinking: Vec::new(),
        structured: Some(value),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn text(answer: &str) -> ChatResponse {
    ChatResponse {
        content: Some(answer.to_string()),
        tool_calls: vec![],
        thinking: Vec::new(),
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
    registry: Arc<dyn ToolRegistry>,
    max_attempts: u32,
) -> (Pipeline, Arc<ScriptedProvider>) {
    pipeline_with_named(responses, registry, max_attempts, Default::default())
}

/// Like [`pipeline`], but seeds `[models.named]` entries so gate-model
/// overrides can be exercised. Each named entry maps to the same mock
/// provider but a distinct model string, which the scripted provider
/// records on the request.
fn pipeline_with_named(
    responses: Vec<ChatResponse>,
    registry: Arc<dyn ToolRegistry>,
    max_attempts: u32,
    named: std::collections::BTreeMap<String, ModelChoice>,
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
            description: None,
            fallbacks: Vec::new(),
        }),
        named,
        ..Default::default()
    };
    // The ledger is both the router's meter and the pipeline's tally, exactly
    // as `Runtime` wires it, so tests exercise the real attribution path.
    let usage = Arc::new(crate::usage::UsageLedger::unpriced());
    let router = Arc::new(
        graph_llm::ModelRouter::with_providers(providers, roles).with_meter(usage.clone()),
    );
    (
        Pipeline {
            router,
            registry,
            events: Arc::new(NullSink),
            plans: Arc::new(Vec::new()),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: "test user".into(),
            current_date: "2026-07-09".into(),
            max_attempts,
            usage,
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
async fn explicit_runs_resolve_tools_against_the_catalog_before_any_step() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({"values": []}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec![],
    });
    let (mut pipeline, provider) = pipeline(vec![], registry.clone(), 1);
    pipeline.catalog = Some(Arc::new(catalog::ToolCatalog {
        mcp_servers: std::collections::BTreeSet::from(["t".to_string()]),
        ..Default::default()
    }));

    // E0 would resolve (server 't' is configured) but E1's server is not —
    // the run must fail before E0 executes, not between E0 and E1.
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "ghost__scan", "input": {}}
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Solve(SolverData::default()), None)
        .await
        .unwrap_err();
    assert!(matches!(err, PipelineError::InvalidPlan(_)), "{err}");
    assert!(err.to_string().contains("ghost"), "{err}");
    assert!(
        registry.invocations.lock().unwrap().is_empty(),
        "no step may execute when a later tool cannot resolve"
    );
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "no LLM spend either"
    );
}

#[test]
fn validate_plan_rejects_workbench_tools_statically() {
    let (pipeline, _) = pipeline(vec![], search_registry(json!({})), 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "workbench__grep", "input": {}}
    ]))
    .unwrap();
    let problems = pipeline.validate_plan(&plan).unwrap_err();
    assert!(problems[0].contains("'workbench__grep'"), "{problems:?}");
    assert!(
        problems[0].contains("not available in the plan runtime"),
        "{problems:?}"
    );

    // The same guard applies inside control-step bodies.
    let body_plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": {
            "if": {"value": "{{E0.count}}", "op": "gt", "to": 0},
            "then": {"toolName": "workbench__read_file", "input": {}}
        }}
    ]))
    .unwrap();
    let problems = pipeline.validate_plan(&body_plan).unwrap_err();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("'workbench__read_file'")),
        "{problems:?}"
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

fn named_model(model: &str) -> std::collections::BTreeMap<String, ModelChoice> {
    let mut named = std::collections::BTreeMap::new();
    named.insert(
        "fast".to_string(),
        ModelChoice {
            provider: "mock".to_string(),
            model: model.to_string(),
            temperature: None,
            dimensions: None,
            description: None,
            fallbacks: Vec::new(),
        },
    );
    named
}

#[tokio::test]
async fn inferred_exit_model_override_selects_named_model() {
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (pipeline, provider) = pipeline_with_named(
        vec![structured(json!({"verdict": true, "reason": "ok"}))],
        registry,
        1,
        named_model("haiku-fast"),
    );
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "exit", "input": {
            "infer": "Is this blocked? {{E0.values}}",
            "model": "fast",
            "status": "error",
            "message": "Blocked",
        }}
    ]))
    .unwrap();
    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    // The verdict call used the named model, not the judge/default.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0].model, "haiku-fast");
}

#[tokio::test]
async fn inferred_decide_model_override_selects_named_model() {
    let registry = search_registry(json!({"values": [{"id": 1}]}));
    let (pipeline, provider) = pipeline_with_named(
        vec![structured(json!({"verdict": false, "reason": "no"}))],
        registry,
        1,
        named_model("haiku-fast"),
    );
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": {
            "infer": "Is this urgent? {{E0.values}}",
            "model": "fast",
            "then": {"toolName": "t__search", "input": {"query": "y"}},
        }}
    ]))
    .unwrap();
    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0].model, "haiku-fast");
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
async fn decide_branch_exit_ends_the_plan() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, provider) = pipeline(vec![], registry.clone(), 1);
    // Gate fires (1 > 0); the then-branch does one call, then exits the
    // whole plan.
    let plan = decide_plan(
        json!([
            {"id": "note", "toolName": "t__issues", "input": {"q": "safe"}},
            {"id": "bail", "toolName": "exit",
             "input": {"status": "success", "message": "done early"}},
        ]),
        None,
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let exit = outcome.exit.expect("exited from the branch");
    assert_eq!(exit.status, crate::pipeline::ExitStatus::Success);
    assert_eq!(exit.message, "done early");
    assert_eq!(exit.step, "E1/then/bail");
    assert!(provider.requests.lock().unwrap().is_empty(), "no LLM calls");
}

#[tokio::test]
async fn decide_branch_exit_gate_passes_and_branch_continues() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    // The branch's exit gate does NOT fire (1 > 100 is false), so the
    // branch continues to its next step and the plan completes normally.
    let plan = decide_plan(
        json!([
            {"id": "bail", "toolName": "exit", "input": {
                "when": {"value": "{{E0.values.length}}", "op": "gt", "to": 100},
                "status": "error", "message": "too many"
            }},
            {"id": "note", "toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}},
        ]),
        None,
    );
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    assert!(outcome.exit.is_none(), "gate passed — no exit");
    let decision = &outcome.state.results["E1"];
    assert_eq!(decision["branch"], json!("then"));
    assert_eq!(decision["result"], json!({"got": {"q": "team-1"}}));
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
fn decide_doc_accepts_exit_in_branch_but_not_nested_control() {
    // Exit in a branch is a supported pattern (it ends the whole plan)…
    let doc: crate::pipeline::doc::PlanDoc = serde_yaml::from_str(
        r#"
identifier: ok
name: Ok
description: exit in a branch
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
    crate::pipeline::doc::validate_doc(&doc).unwrap();

    // …nested decide/map/reduce still are not.
    let doc: crate::pipeline::doc::PlanDoc = serde_yaml::from_str(
        r#"
identifier: bad
name: Bad
description: map nested in branch
steps:
  - id: E0
    tool_name: decide
    input:
      if: { value: 1, op: eq, to: 1 }
      then:
        tool_name: map
        input: {}
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
async fn filter_where_partitions_in_input_order() {
    let registry = search_registry(json!({"changes": [
        {"path": "a.rs", "status": "modified"},
        {"path": "b.json", "status": "deleted"},
        {"path": "c.md", "status": "added"},
    ]}));
    let (pipeline, provider) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "filter", "input": {
            "over": "{{E0.changes}}",
            "where": {"value": "{{item.status}}", "op": "ne", "to": "deleted"},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let filter = &outcome.state.results["E1"];
    assert_eq!(filter["count"], json!(2));
    assert_eq!(
        filter["items"],
        json!([
            {"path": "a.rs", "status": "modified"},
            {"path": "c.md", "status": "added"},
        ]),
        "kept elements, input order"
    );
    assert_eq!(filter["dropped_count"], json!(1));
    assert_eq!(
        filter["dropped"],
        json!([{"path": "b.json", "status": "deleted"}]),
        "both halves of the partition are addressable"
    );
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "`where` costs no LLM calls"
    );
}

#[tokio::test]
async fn filter_infer_judges_each_item() {
    let registry = search_registry(json!({"values": [
        {"title": "fix the bug"},
        {"title": "update readme"},
        {"title": "fix the crash"},
    ]}));
    let (pipeline, provider) = pipeline(
        vec![
            structured(json!({"verdict": true, "reason": "code change"})),
            structured(json!({"verdict": false, "reason": "docs only"})),
            structured(json!({"verdict": true, "reason": "code change"})),
        ],
        registry,
        1,
    );
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "filter", "input": {
            "over": "{{E0.values}}",
            "infer": "Is \"{{item.title}}\" a code change?",
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let filter = &outcome.state.results["E1"];
    assert_eq!(filter["count"], json!(2));
    assert_eq!(
        filter["items"],
        json!([{"title": "fix the bug"}, {"title": "fix the crash"}]),
    );
    assert_eq!(filter["dropped"], json!([{"title": "update readme"}]));
    // Each verdict saw its own item, rendered.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "one judge call per item");
    assert!(matches!(
        &requests[1].messages[0],
        graph_llm::types::ChatMessage::User { content } if content.contains("update readme")
    ));
}

#[tokio::test]
async fn concurrent_filter_infer_judges_every_item() {
    let values: Vec<Value> = (0..5).map(|n| json!({"id": format!("v{n}")})).collect();
    let registry = search_registry(json!({"values": values}));
    // Identical verdicts: partition is order-independent under concurrency.
    let verdicts = (0..5)
        .map(|_| structured(json!({"verdict": true, "reason": "keep"})))
        .collect();
    let (pipeline, provider) = pipeline(verdicts, registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "filter", "input": {
            "over": "{{E0.values}}",
            "infer": "keep {{item.id}}?",
            "concurrency": 3,
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let filter = &outcome.state.results["E1"];
    assert_eq!(filter["count"], json!(5), "every item judged and kept");
    assert_eq!(filter["dropped_count"], json!(0));
    let expected: Vec<Value> = (0..5).map(|n| json!({"id": format!("v{n}")})).collect();
    assert_eq!(
        filter["items"],
        json!(expected),
        "input order regardless of concurrency"
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn filter_infer_model_override_selects_named_model_for_every_verdict() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (pipeline, provider) = pipeline_with_named(
        vec![
            structured(json!({"verdict": true, "reason": "yes"})),
            structured(json!({"verdict": false, "reason": "no"})),
        ],
        registry,
        1,
        named_model("haiku-fast"),
    );
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "filter", "input": {
            "over": "{{E0.values}}",
            "infer": "keep {{item.id}}?",
            "model": "fast",
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E1"]["count"], json!(1));
    // Every per-item verdict used the named model, not the judge/default.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.model, "haiku-fast");
    }
}

#[tokio::test]
async fn filter_over_empty_list_is_a_value_not_an_error() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "filter", "input": {
            "over": "{{E0.values}}",
            "infer": "keep {{item}}?",
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let filter = &outcome.state.results["E1"];
    assert_eq!(filter["count"], json!(0));
    assert_eq!(filter["items"], json!([]));
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "no items, no judge calls"
    );
}

#[tokio::test]
async fn filter_where_on_a_missing_field_fails_hard() {
    let registry = search_registry(json!({"values": [{"id": "a"}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "filter", "input": {
            "over": "{{E0.values}}",
            "where": {"value": "{{item.status}}", "op": "ne", "to": "deleted"},
        }},
    ]))
    .unwrap();
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    match err {
        PipelineError::StepFailed { step, tool, .. } => {
            assert_eq!(step, "E1");
            assert_eq!(tool, "filter");
        }
        other => panic!("expected StepFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn filter_nested_in_map_body_shadows_the_outer_item() {
    let registry = search_registry(json!({"values": [
        {"id": "p1", "children": [{"size": 3}, {"size": 0}]},
        {"id": "p2", "children": [{"size": 0}]},
    ]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    // The nested filter's `over` reads the OUTER {{item.children}}; its
    // `where` reads the INNER {{item.size}} — shadowed per candidate.
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "filter", "input": {
                "over": "{{item.children}}",
                "where": {"value": "{{item.size}}", "op": "gt", "to": 0},
            }},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let map = &outcome.state.results["E1"];
    assert_eq!(map["count"], json!(2));
    assert_eq!(map["results"][0]["items"], json!([{"size": 3}]));
    assert_eq!(map["results"][0]["dropped"], json!([{"size": 0}]));
    assert_eq!(map["results"][1]["count"], json!(0), "p2 keeps nothing");
}

#[tokio::test]
async fn filter_in_decide_branch_and_planner_catalog() {
    let registry = search_registry(json!({"values": [{"n": 1}, {"n": 5}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "decide", "input": {
            "if": {"value": "{{E0.values.length}}", "op": "gt", "to": 0},
            "then": {"toolName": "filter", "input": {
                "over": "{{E0.values}}",
                "where": {"value": "{{item.n}}", "op": "gt", "to": 2},
            }},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let decide = &outcome.state.results["E1"];
    assert_eq!(decide["branch"], json!("then"));
    assert_eq!(decide["result"]["items"], json!([{"n": 5}]));

    // The planner is offered the filter step alongside its siblings.
    let (tools_text, _) = pipeline.planner_catalog().await;
    assert!(
        tools_text.contains("\"name\":\"filter\""),
        "planner catalog lists filter"
    );
}

/// Registry with the mock tools plus the real `data` pack, so
/// `builtin__reshape` runs its actual code path behind the pipeline's
/// render.
fn registry_with_data_pack(values: Value) -> Arc<dyn ToolRegistry> {
    let docs = crate::user_tools::load_pack_tools(&["data".to_string()]).unwrap();
    let providers: std::collections::HashMap<String, Arc<dyn ChatProvider>> =
        std::collections::HashMap::new();
    let router = Arc::new(graph_llm::ModelRouter::with_providers(
        providers,
        ModelRoles::default(),
    ));
    Arc::new(crate::tools::CompositeRegistry::new(vec![
        search_registry(values),
        Arc::new(crate::user_tools::UserToolRegistry::builtins(docs, router)),
    ]))
}

#[tokio::test]
async fn caller_shape_survives_model_authored_template_syntax() {
    // The regression: the pipeline renders a `map` body's call input against
    // the full scope, so `{{item.text}}` is already substituted when
    // `builtin__reshape` is dispatched. A second, tool-level render would
    // parse that substituted text — here an LLM quoting Helm, Actions, and
    // mustache tags — as graph templates and fail the step.
    let items = json!({"values": [
        {"text": "quoting a helm chart: {{ index .Values.image.tag }}"},
        {"text": "and an action: ${{ github.token }}"},
        {"text": "and mustache: {{#section}}body{{/section}}"},
    ]});
    let registry = registry_with_data_pack(items.clone());
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "builtin__reshape", "input": {
                "shape": {"body": "{{item.text}}"},
            }},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let bodies: Vec<Value> = items["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| json!({"body": item["text"]}))
        .collect();
    assert_eq!(
        outcome.state.results["E1"]["results"],
        Value::Array(bodies),
        "every field passes through byte-identical"
    );
}

#[tokio::test]
async fn caller_shape_resolves_step_and_input_roots() {
    // The other half of the contract: the pipeline pass is what resolves a
    // caller-supplied shape, and it sees the whole scope — earlier results
    // and the plan's inputs — with the typed splice intact.
    let registry = registry_with_data_pack(json!({"number": 12, "labels": ["bug"]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "builtin__reshape", "input": {"shape": {
            "pr": "{{E0.number}}",
            "tags": "{{E0.labels}}",
            "title": "PR #{{E0.number}} for {{input.repo}}",
        }}},
    ]))
    .unwrap();
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, Some(json!({"repo": "graph"})))
        .await
        .unwrap();
    assert_eq!(
        outcome.state.results["E1"],
        json!({
            "pr": 12,                            // exact tag keeps the number
            "tags": ["bug"],                     // exact tag keeps the array
            "title": "PR #12 for graph",         // mixed text interpolates
        })
    );
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
    /// The scope map handed to each consultation, in order.
    scopes: Mutex<Vec<Map<String, Value>>>,
}

impl ScriptedGate {
    fn new(decisions: Vec<GateDecision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions),
            seen: Mutex::new(Vec::new()),
            scopes: Mutex::new(Vec::new()),
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
        self.scopes.lock().unwrap().push(ctx.scope.clone());
        let mut decisions = self.decisions.lock().unwrap();
        if decisions.is_empty() {
            GateDecision::Proceed
        } else {
            decisions.remove(0)
        }
    }
}

/// Sink capturing step_finished events: (path, tool, result, is_error).
/// Also records the drafting-progress events as (name, detail) pairs.
#[derive(Default)]
struct RecordingSink {
    finished: Mutex<Vec<(String, String, Value, bool)>>,
    drafting: Mutex<Vec<(String, Value)>>,
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

    fn planning(&self) {
        self.drafting
            .lock()
            .unwrap()
            .push(("planning".to_string(), Value::Null));
    }

    fn draft_outline(&self, items: &Value) {
        self.drafting
            .lock()
            .unwrap()
            .push(("draft_outline".to_string(), items.clone()));
    }

    fn draft_step_started(&self, index: usize, summary: &str) {
        self.drafting.lock().unwrap().push((
            "draft_step_started".to_string(),
            json!({"index": index, "summary": summary}),
        ));
    }

    fn draft_step_finished(&self, index: usize, step: &Value, problems: &[String], attempt: u32) {
        self.drafting.lock().unwrap().push((
            "draft_step_finished".to_string(),
            json!({"index": index, "step": step, "problems": problems, "attempt": attempt}),
        ));
    }
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
    let PipelineError::Aborted { step, error, state } = err else {
        panic!("expected Aborted, got {err}");
    };
    assert_eq!(step, "E0");
    assert!(
        error.is_none(),
        "a pre-dispatch breakpoint abort carries no tool error"
    );
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

// ── Gate scope + pause-on-error ──────────────────────────────────────────

/// Gate that consumes scripted error decisions (exhausted = Fail) and
/// proceeds every before_tool consult.
struct ErrorGate {
    decisions: Mutex<Vec<ErrorDecision>>,
    errors_seen: Mutex<Vec<(String, Value)>>,
}

impl ErrorGate {
    fn new(decisions: Vec<ErrorDecision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions),
            errors_seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ExecutionGate for ErrorGate {
    async fn before_tool(&self, _ctx: GateContext<'_>) -> GateDecision {
        GateDecision::Proceed
    }

    async fn on_tool_error(&self, ctx: GateContext<'_>, error: &Value) -> ErrorDecision {
        self.errors_seen
            .lock()
            .unwrap()
            .push((ctx.path.to_string(), error.clone()));
        let mut decisions = self.decisions.lock().unwrap();
        if decisions.is_empty() {
            ErrorDecision::Fail
        } else {
            decisions.remove(0)
        }
    }
}

#[tokio::test]
async fn gate_sees_top_level_scope() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let gate = ScriptedGate::new(vec![]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}},
    ]))
    .unwrap();
    pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, Some(json!({"team": "core"})))
        .await
        .unwrap();
    let scopes = gate.scopes.lock().unwrap();
    assert_eq!(scopes[0].get("input"), Some(&json!({"team": "core"})));
    assert!(!scopes[0].contains_key("E0"), "E0 has not run yet");
    assert_eq!(
        scopes[1].get("E0"),
        Some(&json!({"values": [{"id": "team-1"}]})),
        "the second consult sees the first step's result"
    );
}

#[tokio::test]
async fn gate_sees_body_scope_pseudo_roots() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let gate = ScriptedGate::new(vec![]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "reduce", "input": {
            "over": "{{E0.values}}",
            "initial": 0,
            "do": [
                {"id": "E10", "toolName": "t__issues", "input": {"q": "{{item.id}}"}},
                {"id": "E11", "toolName": "t__issues", "input": {"prior": "{{E10.got.q}}"}},
            ],
        }},
    ]))
    .unwrap();
    pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let scopes = gate.scopes.lock().unwrap();
    // Consult order: E0, then per item: E1/do.N/E10, E1/do.N/E11.
    let first_body = &scopes[1];
    assert_eq!(first_body.get("item"), Some(&json!({"id": "a"})));
    assert_eq!(first_body.get("index"), Some(&json!(0)));
    assert_eq!(first_body.get("accumulator"), Some(&json!(0)));
    assert!(first_body.contains_key("E0"), "base results are layered in");
    let second_body_step = &scopes[2];
    assert_eq!(
        second_body_step.get("E10"),
        Some(&json!({"got": {"q": "a"}})),
        "earlier same-body step results are in scope"
    );
}

#[tokio::test]
async fn gate_scope_inside_nested_plan_is_the_nested_results() {
    let inner = plan_doc_yaml(
        r#"
identifier: inner
name: Inner
description: inner plan
input_schema:
  type: object
  properties:
    tag: { type: string }
steps:
  - id: E0
    tool_name: t__search
    input: { query: "{{input.tag}}" }
"#,
    );
    let registry = search_registry(json!({"values": []}));
    let (mut pipeline, _) = pipeline(vec![], registry, 1);
    pipeline.plans = Arc::new(vec![inner]);
    let gate = ScriptedGate::new(vec![]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "plan__inner", "input": {"tag": "x"}},
    ]))
    .unwrap();
    pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    let seen = gate.seen.lock().unwrap();
    let scopes = gate.scopes.lock().unwrap();
    assert_eq!(seen[1].0, vec!["inner".to_string()]);
    assert_eq!(
        scopes[1].get("input"),
        Some(&json!({"tag": "x"})),
        "nested consult sees the nested plan's own input, not the outer results"
    );
}

#[tokio::test]
async fn on_tool_error_default_fail_preserves_behavior() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__search".to_string()],
    });
    let (pipeline, _) = pipeline(vec![], registry, 1);
    // ScriptedGate implements only before_tool — default on_tool_error.
    let gate = ScriptedGate::new(vec![]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
    ]))
    .unwrap();
    let err = pipeline
        .with_gate(gate)
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let PipelineError::StepFailed { step, message, .. } = err else {
        panic!("expected StepFailed, got {err}");
    };
    assert_eq!(step, "E0");
    assert!(message.contains("boom"), "{message}");
}

#[tokio::test]
async fn on_tool_error_replace_substitutes_and_continues() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__search".to_string()],
    });
    let (mut pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let sink = Arc::new(RecordingSink::default());
    pipeline.events = sink.clone();
    let replacement = json!({"values": [{"id": "patched"}]});
    let gate = ErrorGate::new(vec![ErrorDecision::Replace {
        result: replacement.clone(),
    }]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "t__issues", "input": {"q": "{{E0.values.0.id}}"}},
    ]))
    .unwrap();
    let outcome = pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E0"], replacement);
    let invocations = registry.invocations.lock().unwrap();
    assert_eq!(
        invocations[1].1,
        json!({"q": "patched"}),
        "downstream template consumed the replacement"
    );
    // The gate saw the real error.
    let errors = gate.errors_seen.lock().unwrap();
    assert_eq!(errors[0].0, "E0");
    assert!(errors[0].1.to_string().contains("boom"));
    // Event order: step_finished carries the resolution, not the failure.
    let finished = sink.finished.lock().unwrap();
    let e0 = finished.iter().find(|(p, ..)| p == "E0").unwrap();
    assert_eq!(e0.2, replacement);
    assert!(!e0.3, "resolved step is not an error");
}

#[tokio::test]
async fn on_tool_error_abort_is_hard() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__search".to_string()],
    });
    let (pipeline, provider) = pipeline(
        vec![
            structured(two_step_plan("E0.values.0.id")),
            structured(two_step_plan("E0.values.0.id")),
        ],
        registry,
        2,
    );
    let gate = ErrorGate::new(vec![ErrorDecision::Abort]);
    let err = pipeline
        .with_gate(gate)
        .run_planned("sprint status")
        .await
        .unwrap_err();
    let PipelineError::Aborted { step, error, .. } = err else {
        panic!("expected Aborted, got {err}");
    };
    assert_eq!(step, "E0");
    assert_eq!(
        error,
        Some(json!({"error": "boom"})),
        "a tool-error abort carries the failing tool's error so the caller can troubleshoot"
    );
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1,
        "no replan, no error summary"
    );
}

#[tokio::test]
async fn on_tool_error_replace_in_planned_mode_skips_replan() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__search".to_string()],
    });
    let (pipeline, provider) = pipeline(
        vec![structured(two_step_plan("E0.values.0.id")), text("done")],
        registry,
        3,
    );
    let gate = ErrorGate::new(vec![ErrorDecision::Replace {
        result: json!({"values": [{"id": "patched"}]}),
    }]);
    let outcome = pipeline
        .with_gate(gate)
        .run_planned("sprint status")
        .await
        .unwrap();
    assert_eq!(outcome.answer, "done");
    assert_eq!(outcome.state.plan_attempts, 1, "no replan happened");
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        2,
        "planner + solver only"
    );
}

#[tokio::test]
async fn on_tool_error_fires_inside_map_body_but_not_for_nested_aborts() {
    // Inside a map body: the error consult fires with the body path.
    let registry = Arc::new(MockRegistry {
        search_result: json!({"values": [{"id": "a"}]}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__issues".to_string()],
    });
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let gate = ErrorGate::new(vec![ErrorDecision::Replace {
        result: json!("ok"),
    }]);
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {"toolName": "t__issues", "input": {"q": "{{item.id}}"}},
        }},
    ]))
    .unwrap();
    let outcome = pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E1"]["results"][0], json!("ok"));
    assert_eq!(gate.errors_seen.lock().unwrap()[0].0, "E1/do.0");

    // A nested-plan ABORT must not be re-asked as an error.
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
    let registry = search_registry(json!({"values": []}));
    let (mut nested_pipeline, _) = super::tests::pipeline(vec![], registry, 1);
    nested_pipeline.plans = Arc::new(vec![inner]);
    // Abort the inner call at its before_tool; outer must propagate the
    // abort without consulting on_tool_error.
    struct AbortInnerGate {
        error_consults: Mutex<u32>,
    }
    #[async_trait]
    impl ExecutionGate for AbortInnerGate {
        async fn before_tool(&self, ctx: GateContext<'_>) -> GateDecision {
            if ctx.call_stack.is_empty() {
                GateDecision::Proceed
            } else {
                GateDecision::Abort
            }
        }
        async fn on_tool_error(&self, _ctx: GateContext<'_>, _e: &Value) -> ErrorDecision {
            *self.error_consults.lock().unwrap() += 1;
            ErrorDecision::Fail
        }
    }
    let gate = Arc::new(AbortInnerGate {
        error_consults: Mutex::new(0),
    });
    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "plan__inner", "input": {}},
    ]))
    .unwrap();
    let err = nested_pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(matches!(err, PipelineError::Aborted { .. }), "{err}");
    assert_eq!(
        *gate.error_consults.lock().unwrap(),
        0,
        "nested aborts are never re-asked as errors"
    );
}

// ── Plan drafting ────────────────────────────────────────────────────────

fn outline_response() -> ChatResponse {
    structured(json!({
        "items": [
            {"summary": "find the team", "expectedTool": "t__search"},
            {"summary": "fetch its issues", "expectedTool": "t__issues"},
        ],
        "queryToAnswer": "how is the sprint going",
    }))
}

fn step_draft(step: Value, plan_complete: bool) -> ChatResponse {
    structured(json!({"step": step, "planComplete": plan_complete}))
}

fn search_step(id: &str) -> Value {
    json!({"id": id, "toolName": "t__search", "input": {"query": "platform"}})
}

fn issues_step(id: &str, reference: &str) -> Value {
    json!({"id": id, "toolName": "t__issues", "input": {"teamId": format!("{{{{{reference}}}}}")}})
}

fn assistant_turns(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            graph_llm::types::ChatMessage::Assistant {
                content: Some(content),
                ..
            } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn draft_generates_outline_then_steps() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![
            outline_response(),
            step_draft(search_step("E0"), false),
            step_draft(issues_step("E1", "E0.values.0.id"), true),
        ],
        registry.clone(),
        1,
    );
    let output = pipeline.draft_plan("sprint status", None).await.unwrap();

    let ids: Vec<&str> = output.plan.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["E0", "E1"]);
    assert_eq!(
        output.solver_data.query_to_answer,
        "how is the sprint going"
    );
    assert!(
        !output.solver_data.data.is_empty(),
        "default solver data filled from the plan"
    );
    assert!(
        registry.invocations.lock().unwrap().is_empty(),
        "drafting must not execute"
    );

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "outline + one call per step");
    // The prompt-cache invariant: one byte-identical system prompt.
    assert!(
        requests.iter().all(|r| r.system == requests[0].system),
        "every call must reuse the identical system prompt"
    );
    // The last step call sees the outline and the accepted E0 as
    // Assistant turns in the scratchpad.
    let assistants = assistant_turns(&requests[2]);
    assert_eq!(assistants.len(), 2, "outline + accepted E0");
    assert!(assistants[0].contains("find the team"), "{assistants:?}");
    assert!(assistants[1].contains("t__search"), "{assistants:?}");
}

#[tokio::test]
async fn draft_retries_invalid_step_with_errors_injected() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![
            outline_response(),
            step_draft(search_step("E0"), false),
            step_draft(issues_step("E1", "E9.values"), false), // E9 does not exist
            step_draft(issues_step("E1", "E0.values.0.id"), false),
            step_draft(search_step("E2"), true),
        ],
        registry,
        1,
    );
    let output = pipeline.draft_plan("sprint status", None).await.unwrap();
    let ids: Vec<&str> = output.plan.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["E0", "E1", "E2"]);

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 5, "one retry for the invalid step");
    // The retry request carries the invalid attempt and the problem text.
    let retry = &requests[3];
    let assistants = assistant_turns(retry);
    assert!(
        assistants.iter().any(|turn| turn.contains("E9")),
        "the invalid StepDraft is in the retry tail: {assistants:?}"
    );
    let users: Vec<String> = retry
        .messages
        .iter()
        .filter_map(|message| match message {
            graph_llm::types::ChatMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        users
            .iter()
            .any(|turn| turn.contains("The step is invalid") && turn.contains("E9")),
        "the validation problem is injected as feedback: {users:?}"
    );
    // After acceptance the failed attempt is dropped from persistent
    // history: the next step call must not carry it.
    let after = assistant_turns(&requests[4]);
    assert!(
        after.iter().all(|turn| !turn.contains("E9")),
        "the retry tail must be discarded on acceptance: {after:?}"
    );
}

#[tokio::test]
async fn draft_exhausted_retries_returns_valid_partial() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(
        vec![
            outline_response(),
            step_draft(search_step("E0"), false),
            step_draft(issues_step("E1", "E9.values"), false),
            step_draft(issues_step("E1", "E8.values"), false),
            step_draft(issues_step("E1", "E7.values"), false),
        ],
        registry,
        1,
    );
    let err = pipeline
        .draft_plan("sprint status", None)
        .await
        .unwrap_err();
    let PipelineError::DraftStepExhausted {
        step_id,
        attempts,
        problems,
        partial,
    } = err
    else {
        panic!("expected DraftStepExhausted");
    };
    assert_eq!(step_id, "E1");
    assert_eq!(attempts, 3);
    assert!(problems.iter().any(|p| p.contains("E7")), "{problems:?}");
    let ids: Vec<&str> = partial.plan.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["E0"], "the valid prefix is carried out");
    assert_eq!(
        partial.solver_data.query_to_answer,
        "how is the sprint going"
    );
}

#[tokio::test]
async fn drafting_into_an_existing_plan_carries_it_in_the_system_prompt() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![outline_response(), step_draft(search_step("E0"), true)],
        registry,
        1,
    );
    let existing: PlannerOutput = serde_json::from_value(two_step_plan("E0.values.0.id")).unwrap();
    pipeline
        .draft_plan("also fetch comments", Some(&existing))
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let system = &requests[0].system;
    assert!(system.contains("Draft Under Revision"), "revision section");
    assert!(system.contains("t__search"), "serialized draft in prompt");
    // There is no caller-supplied feedback slot: steering the planner at a
    // plan it will wholly replace is what the editing commands are for.
    assert!(
        !system.contains("Last Error"),
        "the drafting prompt has no last-error slot: {system}"
    );
    // Constant across the session: the step call sees the same system.
    assert_eq!(requests[0].system, requests[1].system);
}

#[tokio::test]
async fn draft_accepts_done_early_without_a_step() {
    // One accepted step, then step: null + planComplete → a 1-step plan.
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![
            outline_response(),
            step_draft(search_step("E0"), false),
            structured(json!({"step": null, "planComplete": true})),
        ],
        registry.clone(),
        1,
    );
    let output = pipeline.draft_plan("sprint status", None).await.unwrap();
    assert_eq!(output.plan.len(), 1);
    assert_eq!(output.plan[0].id, "E0");
    assert_eq!(provider.requests.lock().unwrap().len(), 3);

    // step: null on an EMPTY plan is a defect: retried, never accepted.
    let (empty_pipeline, provider) = super::tests::pipeline(
        vec![
            outline_response(),
            structured(json!({"step": null, "planComplete": true})),
            step_draft(search_step("E0"), true),
        ],
        registry,
        1,
    );
    let output = empty_pipeline
        .draft_plan("sprint status", None)
        .await
        .unwrap();
    assert_eq!(output.plan.len(), 1, "the retry produced the real step");
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        3,
        "outline + rejected null + retry"
    );
}

#[tokio::test]
async fn draft_emits_progress_events() {
    let registry = search_registry(json!({"values": []}));
    let (mut pipeline, _) = pipeline(
        vec![
            outline_response(),
            step_draft(search_step("E0"), false),
            step_draft(issues_step("E1", "E9.values"), false), // invalid → retry
            step_draft(issues_step("E1", "E0.values.0.id"), true),
        ],
        registry,
        1,
    );
    let sink = Arc::new(RecordingSink::default());
    pipeline.events = sink.clone();
    pipeline.draft_plan("sprint status", None).await.unwrap();

    let events = sink.drafting.lock().unwrap();
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "planning",
            "draft_outline",
            "draft_step_started",
            "draft_step_finished", // E0 accepted
            "draft_step_started",
            "draft_step_finished", // E1 attempt 1 failed
            "draft_step_finished", // E1 attempt 2 accepted
        ]
    );
    assert_eq!(events[1].1.as_array().unwrap().len(), 2, "outline items");
    // The failed attempt carries non-empty problems and its attempt number.
    let failed = &events[5].1;
    assert!(!failed["problems"].as_array().unwrap().is_empty());
    assert_eq!(failed["attempt"], json!(1));
    // The accepted retry has empty problems and attempt 2.
    let accepted = &events[6].1;
    assert!(accepted["problems"].as_array().unwrap().is_empty());
    assert_eq!(accepted["attempt"], json!(2));
    assert_eq!(accepted["step"]["id"], json!("E1"));
}

#[tokio::test]
async fn draft_force_completes_when_outline_is_covered_and_planner_never_signals_done() {
    // A 2-stage outline, then valid steps that NEVER set planComplete. The
    // loop must cover the outline, allow MAX_OVERFLOW_STEPS extra, then
    // force-close — not grind to the step budget or error.
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(
        vec![
            outline_response(),
            step_draft(search_step("E0"), false), // stage 1
            step_draft(search_step("E1"), false), // stage 2
            step_draft(search_step("E2"), false), // overflow 1 (closing)
            step_draft(search_step("E3"), false), // overflow 2 (closing)
                                                  // No more responses: the loop must force-close before asking again.
        ],
        registry.clone(),
        1,
    );
    let output = pipeline.draft_plan("sprint status", None).await.unwrap();

    // 2 stages + MAX_OVERFLOW_STEPS (2) = 4 accepted steps, no more.
    assert!(
        output.plan.len() <= 4,
        "force-close caps the plan at outline + overflow: {}",
        output.plan.len()
    );
    assert_eq!(
        output.plan.len(),
        4,
        "all four scripted steps are accepted before force-close"
    );
    assert!(
        !output.solver_data.data.is_empty(),
        "solver data is assembled from the drafted plan"
    );
    assert!(
        registry.invocations.lock().unwrap().is_empty(),
        "drafting must not execute"
    );

    let requests = provider.requests.lock().unwrap();
    // outline + 4 steps = 5; well under the old max_draft_steps (8) + 1 budget.
    assert_eq!(requests.len(), 5, "outline + four steps, then force-close");
    assert!(
        requests.len() < 9,
        "must not grind toward the step budget: {}",
        requests.len()
    );
    // The overflow (closing) calls use the closing wording, not a stale stage.
    let closing_users: Vec<String> = requests[3]
        .messages
        .iter()
        .chain(requests[4].messages.iter())
        .filter_map(|message| match message {
            graph_llm::types::ChatMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        closing_users
            .iter()
            .any(|turn| turn.contains("Every outline stage now has a step")),
        "closing requests use closing_step_request wording: {closing_users:?}"
    );
}

#[tokio::test]
async fn draft_rejects_an_empty_outline() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(
        vec![structured(json!({"items": [], "queryToAnswer": "q"}))],
        registry,
        1,
    );
    let err = pipeline
        .draft_plan("sprint status", None)
        .await
        .unwrap_err();
    assert!(matches!(err, PipelineError::InvalidPlan(_)), "{err}");
    assert!(err.to_string().contains("outline has no items"));
}

#[test]
fn step_draft_schema_makes_step_nullable() {
    // Watch-item: `Option<Step>` must schema out as nullable so providers
    // accept a null/omitted step for the done-early signal.
    let schema = serde_json::to_value(schemars::schema_for!(super::outline::StepDraft)).unwrap();
    let step = &schema["properties"]["step"];
    let text = step.to_string();
    assert!(
        text.contains("null") || text.contains("anyOf"),
        "step must admit null: {text}"
    );
}

// ── agent control step ────────────────────────────────────────────────

fn tool_use(id: &str, name: &str, args: Value) -> ChatResponse {
    ChatResponse {
        thinking: Vec::new(),
        content: None,
        tool_calls: vec![graph_llm::types::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }],
        structured: None,
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

fn agent_step(id: &str, input: Value) -> Value {
    json!({"id": id, "toolName": "agent", "input": input})
}

const OUT_SCHEMA: fn() -> Value = || {
    json!({
        "type": "object",
        "required": ["found"],
        "properties": {"found": {"type": "integer"}}
    })
};

#[tokio::test]
async fn agent_calls_a_tool_then_returns_structured_output() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, _) = pipeline(
        vec![
            tool_use("c1", "t__search", json!({"query": "x"})),
            text(r#"{"found": 1}"#),
        ],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({
            "prompt": "find things",
            "outputSchema": OUT_SCHEMA(),
            "tools": ["t__*"]
        })
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let result = &outcome.state.results["E0"];
    assert_eq!(result["output"], json!({"found": 1}));
    assert_eq!(result["final"], json!(true));
    assert_eq!(result["iterations"], json!(2));
    assert_eq!(result["tools_called"][0]["tool"], "t__search");

    // The tool really ran, through the registry.
    assert_eq!(registry.invocations.lock().unwrap()[0].0, "t__search");
}

/// The agent is told "conform to the output schema provided" — so the schema
/// has to actually be provided. It can reach the model two ways: natively via
/// `response_schema`, or as text the model can read. If neither happens the
/// model is guessing field names, every guess fails validation, and each
/// failure burns a round — which is what an agent that always exhausts its
/// budget looks like from the outside.
#[tokio::test]
async fn the_final_round_withdraws_tools_and_forces_the_schema() {
    let registry = search_registry(json!({"values": []}));
    // Budget of 2: round 1 calls a tool, round 2 is the forced answer round.
    let (pipeline, provider) = pipeline(
        vec![
            tool_use("c1", "t__search", json!({"query": "x"})),
            text(r#"{"found": 0}"#),
        ],
        registry,
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({
            "prompt": "look",
            "outputSchema": OUT_SCHEMA(),
            "tools": ["t__*"],
            "maxIterations": 2
        })
    )]))
    .unwrap();

    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[0].tools.is_empty(),
        "ordinary rounds keep their tools"
    );
    assert!(requests[0].response_schema.is_none());

    // The answer round: no tools to reach for, and the provider is told to
    // force the shape rather than hope for it.
    assert!(
        requests[1].tools.is_empty(),
        "the final round must withdraw tools — a forced schema makes them \
         unreachable anyway"
    );
    let forced = requests[1]
        .response_schema
        .as_ref()
        .expect("final round forces the output schema");
    assert_eq!(forced.schema, OUT_SCHEMA());
    // And the model is told, not just silently constrained.
    assert!(requests[1].messages.iter().any(|m| matches!(
        m, graph_llm::types::ChatMessage::User { content } if content.contains("final turn")
    )));

    // The budget is never named in the system prompt: a number reads as an
    // allowance to spend, and competes with what the plan's own prompt says
    // about how much ground to cover. The final-turn notice is the backstop.
    assert!(
        !requests[0].system.contains("2 round") && !requests[0].system.contains("budget"),
        "the system prompt must not state a round budget:\n{}",
        requests[0].system
    );
}

#[tokio::test]
async fn a_forced_final_answer_comes_back_final_and_usable() {
    // What the provider returns when the schema is forced: the object arrives
    // in `structured`, already validated, with no text to parse.
    let registry = search_registry(json!({"values": []}));
    let forced = ChatResponse {
        content: None,
        tool_calls: vec![],
        thinking: vec![],
        structured: Some(json!({"found": 7})),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    };
    let (pipeline, _) = pipeline(
        vec![tool_use("c1", "t__search", json!({"query": "x"})), forced],
        registry,
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({
            "prompt": "look",
            "outputSchema": OUT_SCHEMA(),
            "tools": ["t__*"],
            "maxIterations": 2
        })
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let result = &outcome.state.results["E0"];
    // Previously this shape — budget spent, tools still wanted — returned
    // final:false with an empty object, and every downstream step that
    // touched `output` failed as a bad path.
    assert_eq!(result["final"], json!(true));
    assert_eq!(result["output"], json!({"found": 7}));
    assert_eq!(result["iterations"], json!(2));
}

#[tokio::test]
async fn the_agent_shows_the_model_its_output_schema() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, provider) = pipeline(vec![text(r#"{"grobnicate": 1}"#)], registry, 1);

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({
            "prompt": "do the thing",
            // A property name that appears nowhere else, so finding it in the
            // request can only mean the schema was sent.
            "outputSchema": {
                "type": "object",
                "required": ["grobnicate"],
                "properties": {"grobnicate": {"type": "integer"}}
            }
        })
    )]))
    .unwrap();

    let _ = pipeline.run_explicit("q", plan, Finish::Silent, None).await;

    let requests = provider.requests.lock().unwrap();
    let first = requests.first().expect("the agent made a call");
    let sent_as_text = first.system.contains("grobnicate")
        || first.messages.iter().any(|m| {
            matches!(m, graph_llm::types::ChatMessage::User { content }
                     if content.contains("grobnicate"))
        });
    let sent_natively = first
        .response_schema
        .as_ref()
        .is_some_and(|s| s.schema.to_string().contains("grobnicate"));

    assert!(
        sent_as_text || sent_natively,
        "the agent never showed the model its output schema — it was asked to \
         match a shape it cannot see.\n  system: {}\n  response_schema: {:?}",
        first.system,
        first.response_schema.as_ref().map(|s| &s.schema),
    );
}

#[tokio::test]
async fn usage_is_attributed_to_the_step_that_spent_it() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, _) = pipeline(
        vec![
            // Two agent rounds…
            tool_use("c1", "t__search", json!({"query": "x"})),
            text(r#"{"found": 1}"#),
            // …then the solver.
            text("the answer"),
        ],
        registry,
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({
            "prompt": "find things",
            "outputSchema": OUT_SCHEMA(),
            "tools": ["t__*"]
        })
    )]))
    .unwrap();

    pipeline
        .run_explicit("q", plan, Finish::Solve(SolverData::default()), None)
        .await
        .unwrap();

    let report = pipeline.usage.take();
    assert_eq!(report.calls, 3, "two agent rounds plus the solver");

    let by_step: std::collections::HashMap<&str, usize> = report
        .by_step
        .iter()
        .map(|s| (s.path.as_str(), s.calls))
        .collect();
    // The agent's rounds land on its step id, not in an unattributed bucket —
    // this is what makes "the scouts cost most of the run" a readable fact
    // rather than an inference.
    assert_eq!(by_step.get("E0"), Some(&2));
    assert_eq!(by_step.get("solver"), Some(&1));
    assert!(
        !by_step.contains_key("unknown"),
        "every pipeline inference site should be scoped: {by_step:?}"
    );
}

#[tokio::test]
async fn nested_plan_usage_rolls_up_into_its_caller() {
    // `graph_review_9000` calls `plan__graph_review_core`; the caller has to
    // see the callee's spend or the composed plan reports almost nothing.
    let registry = search_registry(json!({"values": []}));
    let inner = plan_doc_yaml(
        r#"
identifier: inner
name: Inner
description: an agent step in a callee plan
steps:
  - id: E0
    tool_name: agent
    input:
      prompt: inner work
      output_schema:
        type: object
        required: [found]
        properties:
          found: { type: integer }
    reasoning: the callee's own spend
output:
  found: '{{E0.output.found}}'
"#,
    );
    let (mut pipeline, _) = pipeline(
        vec![text(r#"{"found": 2}"#), text(r#"{"found": 3}"#)],
        registry,
        1,
    );
    pipeline.plans = Arc::new(vec![inner]);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "plan__inner", "input": {}},
        agent_step("E1", json!({"prompt": "outer", "outputSchema": OUT_SCHEMA()})),
    ]))
    .unwrap();

    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let report = pipeline.usage.take();
    assert_eq!(report.calls, 2, "the inner plan's call is not lost");
    let paths: Vec<&str> = report.by_step.iter().map(|s| s.path.as_str()).collect();
    // Qualified by the plan it ran in, so the inner E0 stays distinct from
    // the outer plan's own steps.
    assert!(paths.contains(&"inner/E0"), "got {paths:?}");
    assert!(paths.contains(&"E1"), "got {paths:?}");
}

#[tokio::test]
async fn agent_prompt_renders_against_prior_step_results() {
    let registry = search_registry(json!({"values": [{"id": "team-7"}]}));
    let (pipeline, provider) = pipeline(vec![text(r#"{"found": 0}"#)], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        agent_step(
            "E1",
            json!({
                "prompt": "team is {{E0.values.0.id}}",
                "outputSchema": OUT_SCHEMA()
            })
        )
    ]))
    .unwrap();

    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[0].messages.iter().any(|m| matches!(
            m, graph_llm::types::ChatMessage::User { content } if content.contains("team-7")
        )),
        "prompt must render against prior results"
    );
}

#[tokio::test]
async fn agent_tool_error_returns_into_the_loop_instead_of_failing_the_step() {
    let registry = Arc::new(MockRegistry {
        search_result: json!({}),
        invocations: Mutex::new(Vec::new()),
        fail_tools: vec!["t__search".to_string()],
    });
    let (pipeline, provider) = pipeline(
        vec![
            tool_use("c1", "t__search", json!({"query": "x"})),
            text(r#"{"found": 0}"#),
        ],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA()})
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    // Step succeeded despite the tool failing...
    assert_eq!(outcome.state.results["E0"]["final"], json!(true));
    // ...and the agent was told about the failure.
    let requests = provider.requests.lock().unwrap();
    let saw_error = requests[1].messages.iter().any(|m| {
        matches!(
            m,
            graph_llm::types::ChatMessage::ToolResult { is_error: true, .. }
        )
    });
    assert!(saw_error, "tool error must flow back into the loop");
}

#[tokio::test]
async fn agent_exhausting_iterations_reports_not_final() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(
        vec![
            tool_use("c1", "t__search", json!({})),
            tool_use("c2", "t__search", json!({})),
        ],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA(), "maxIterations": 2})
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let result = &outcome.state.results["E0"];
    assert_eq!(result["final"], json!(false));
    // Exactly the budget - not budget+1.
    assert_eq!(result["iterations"], json!(2));
    assert_eq!(result["tools_called"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn agent_pattern_matching_no_tool_fails_the_step() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA(), "tools": ["ghost__*"]})
    )]))
    .unwrap();

    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("ghost__*"), "{text}");
}

#[tokio::test]
async fn agent_never_offers_plan_and_execute_to_the_model() {
    let registry = search_registry(json!({}));
    let (pipeline, provider) = pipeline(vec![text(r#"{"found": 0}"#)], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA()})
    )]))
    .unwrap();

    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|t| t.name == "plan_and_execute"),
        "plan_and_execute must never be offered inside an agent"
    );
    assert!(requests[0].tools.iter().any(|t| t.name == "t__search"));
}

#[tokio::test]
async fn agent_runs_inside_a_map_body_with_item_scope() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (pipeline, provider) = pipeline(
        vec![text(r#"{"found": 1}"#), text(r#"{"found": 2}"#)],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {
                "toolName": "agent",
                "input": {
                    "prompt": "handle {{item.id}}",
                    "outputSchema": OUT_SCHEMA()
                }
            }
        }}
    ]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let results = &outcome.state.results["E1"]["results"];
    assert_eq!(results[0]["output"], json!({"found": 1}));
    assert_eq!(results[1]["output"], json!({"found": 2}));

    // {{item}} really reached the agent prompt.
    let requests = provider.requests.lock().unwrap();
    let prompts: Vec<String> = requests
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter_map(|m| match m {
            graph_llm::types::ChatMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        prompts.iter().any(|p| p.contains("handle a")),
        "{prompts:?}"
    );
    assert!(
        prompts.iter().any(|p| p.contains("handle b")),
        "{prompts:?}"
    );
}

#[tokio::test]
async fn agent_can_call_a_plan_tool_because_inner_calls_go_through_dispatch() {
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

    let registry = search_registry(json!({"values": [{"id": "team-9"}]}));
    let (mut pipeline, provider) = pipeline(
        vec![
            tool_use("c1", "plan__inner", json!({"q": "x"})),
            text(r#"{"found": 1}"#),
        ],
        registry.clone(),
        1,
    );
    pipeline.plans = Arc::new(vec![inner]);

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA(), "tools": ["plan__*"]})
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    assert_eq!(outcome.state.results["E0"]["final"], json!(true));

    // The sub-plan really executed: its step hit the registry...
    assert_eq!(registry.invocations.lock().unwrap()[0].0, "t__search");
    // ...and its rendered output came back to the agent as a tool result,
    // rather than an "unknown tool" error from the registry.
    let requests = provider.requests.lock().unwrap();
    let saw_plan_output = requests[1].messages.iter().any(|m| {
        matches!(
            m,
            graph_llm::types::ChatMessage::ToolResult { content, is_error: false, .. }
                if content.to_string().contains("team-9")
        )
    });
    assert!(
        saw_plan_output,
        "plan__* result must flow back into the agent loop"
    );

    // plan__* is advertised to the model.
    assert!(requests[0].tools.iter().any(|t| t.name == "plan__inner"));
}

#[tokio::test]
async fn agent_writes_one_bus_summary_like_its_peer_control_steps() {
    let registry = search_registry(json!({"values": []}));
    let (pipeline, _) = pipeline(
        vec![
            tool_use("c1", "t__search", json!({})),
            text(r#"{"found": 0}"#),
        ],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA()})
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let entries: Vec<&BusEntry> = outcome
        .state
        .bus
        .iter()
        .filter(|e| e.source == "E0")
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "one summary line, not one per round: {entries:?}"
    );
    assert_eq!(entries[0].kind, BusKind::Info);
    assert!(
        entries[0].content.contains("agent: 2 round(s)"),
        "{}",
        entries[0].content
    );
    assert!(
        entries[0].content.contains("1 tool call"),
        "{}",
        entries[0].content
    );
}

#[tokio::test]
async fn an_agent_failure_reaches_the_bus_as_a_replan_eligible_error() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA(), "tools": ["ghost__*"]})
    )]))
    .unwrap();

    // run_explicit surfaces the failure directly; run_planned is what
    // consults the bus, so assert the classification the bus would carry.
    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    match err {
        PipelineError::StepFailed { tool, .. } => assert_eq!(tool, "agent"),
        other => panic!("expected StepFailed, got {other:?}"),
    }
}

/// Inner calls are dispatched at body depth, so their path has to carry the
/// item that made them. Collapsing onto `E1/agent.N/tool` would make every
/// item of a concurrent map indistinguishable in gates and event streams.
#[tokio::test]
async fn agent_inner_call_paths_keep_the_body_location() {
    let registry = search_registry(json!({"values": [{"id": "a"}, {"id": "b"}]}));
    let (pipeline, _) = pipeline(
        vec![
            tool_use("c1", "t__search", json!({})),
            text(r#"{"found": 1}"#),
            tool_use("c2", "t__search", json!({})),
            text(r#"{"found": 2}"#),
        ],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {
                "toolName": "agent",
                "input": {"prompt": "handle {{item.id}}", "outputSchema": OUT_SCHEMA()}
            }
        }}
    ]))
    .unwrap();

    let gate = ScriptedGate::new(vec![]);
    pipeline
        .with_gate(gate.clone())
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let paths = gate.paths();
    assert!(
        paths.contains(&"E1/do.0/agent.1/t__search".to_string()),
        "{paths:?}"
    );
    assert!(
        paths.contains(&"E1/do.1/agent.1/t__search".to_string()),
        "{paths:?}"
    );
}

/// Suppressing `plan_and_execute` from the advertised catalogue is only half
/// the boundary: `dispatch` routes the bare name, so the loop must refuse it
/// even when the model asks for it anyway.
#[tokio::test]
async fn agent_refuses_plan_and_execute_even_when_the_model_asks_for_it() {
    let registry = search_registry(json!({}));
    let (pipeline, provider) = pipeline(
        vec![
            tool_use("c1", "plan_and_execute", json!({"query": "do it all"})),
            text(r#"{"found": 0}"#),
        ],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA()})
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E0"]["final"], json!(true));

    let requests = provider.requests.lock().unwrap();
    // Exactly two rounds: no planner call was made in between.
    assert_eq!(requests.len(), 2, "a nested planner run would add requests");
    let refused = requests[1].messages.iter().any(|m| {
        matches!(
            m,
            graph_llm::types::ChatMessage::ToolResult { content, is_error: true, .. }
                if content.to_string().contains("not available inside an agent")
        )
    });
    assert!(refused, "the refusal must return into the loop as an error");
}

#[tokio::test]
async fn gate_abort_inside_an_agent_is_a_hard_stop() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(
        vec![
            tool_use("c1", "t__search", json!({})),
            text(r#"{"found": 0}"#),
        ],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA()})
    )]))
    .unwrap();

    let err = pipeline
        .with_gate(ScriptedGate::new(vec![GateDecision::Abort]))
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PipelineError::Aborted { .. }),
        "expected Aborted, got {err:?}"
    );
    // The abort landed before the tool ran, and no further round happened.
    assert!(registry.invocations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn agent_output_that_misses_the_schema_gets_one_repair_pass() {
    let registry = search_registry(json!({}));
    let (pipeline, provider) = pipeline(
        // Round one answers with the right shape but the wrong type; the
        // repair role returns the corrected document.
        vec![text(r#"{"found": "1"}"#), structured(json!({"found": 1}))],
        registry.clone(),
        1,
    );

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA()})
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let result = &outcome.state.results["E0"];
    assert_eq!(result["output"], json!({"found": 1}), "repaired in place");
    assert_eq!(result["final"], json!(true));
    // The repair is not a round: one inference round, one repair call.
    assert_eq!(result["iterations"], json!(1));
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[1].response_schema.is_some(),
        "the second call must be the structured repair pass"
    );
}

/// Data running out while rendering an agent's prompt is `EmptyData` at any
/// depth: it degrades, and never becomes a replan-eligible tool failure.
#[tokio::test]
async fn agent_empty_data_in_a_body_prompt_degrades() {
    let registry = search_registry(json!({"values": [{"children": []}]}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {}},
        {"id": "E1", "toolName": "map", "input": {
            "over": "{{E0.values}}",
            "do": {
                "toolName": "agent",
                "input": {
                    "prompt": "handle {{item.children.0.id}}",
                    "outputSchema": OUT_SCHEMA()
                }
            }
        }}
    ]))
    .unwrap();

    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PipelineError::EmptyData { .. }),
        "expected EmptyData, got {err:?}"
    );
}

#[tokio::test]
async fn agent_system_prompt_renders_against_the_scope() {
    let registry = search_registry(json!({"values": [{"id": "team-3"}]}));
    let (pipeline, provider) = pipeline(vec![text(r#"{"found": 0}"#)], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {}},
        agent_step(
            "E1",
            json!({
                "prompt": "p",
                "systemPrompt": "The team is {{E0.values.0.id}}.",
                "outputSchema": OUT_SCHEMA()
            })
        )
    ]))
    .unwrap();

    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[0].system.contains("The team is team-3."),
        "{}",
        requests[0].system
    );
}

#[tokio::test]
async fn an_explicitly_empty_tool_list_fails_the_step_rather_than_granting_everything() {
    let registry = search_registry(json!({}));
    let (pipeline, provider) = pipeline(vec![text(r#"{"found": 0}"#)], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([agent_step(
        "E0",
        json!({"prompt": "p", "outputSchema": OUT_SCHEMA(), "tools": []})
    )]))
    .unwrap();

    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("`tools` is empty"), "{text}");
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "no inference may be spent on it"
    );
}

// ── ask steps ──────────────────────────────────────────────────────────────

/// A scripted human. Records what it was asked so tests can assert the
/// question actually rendered.
struct ScriptedHuman {
    outcome: Mutex<Vec<AskOutcome>>,
    asked: Mutex<Vec<AskRequest>>,
}

impl ScriptedHuman {
    fn new(outcomes: Vec<AskOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(outcomes),
            asked: Mutex::new(Vec::new()),
        })
    }

    fn answering(answer: Value) -> Arc<Self> {
        Self::new(vec![AskOutcome::Answered(answer)])
    }

    fn prompts(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.prompt.clone())
            .collect()
    }
}

#[async_trait]
impl Interlocutor for ScriptedHuman {
    async fn ask(&self, request: AskRequest) -> AskOutcome {
        self.asked.lock().unwrap().push(request);
        let mut outcomes = self.outcome.lock().unwrap();
        if outcomes.len() > 1 {
            outcomes.remove(0)
        } else {
            outcomes
                .first()
                .cloned()
                .unwrap_or(AskOutcome::Unavailable("script exhausted".into()))
        }
    }
}

const ASK_SCHEMA: fn() -> Value = || {
    json!({
        "type": "object",
        "required": ["repo"],
        "properties": {"repo": {"type": "string", "description": "Target repo"}}
    })
};

fn ask_step(id: &str, input: Value) -> Value {
    json!({"id": id, "toolName": "ask", "input": input})
}

#[tokio::test]
async fn an_answer_becomes_the_step_result_and_flows_downstream() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);
    let human = ScriptedHuman::answering(json!({"repo": "tylerdavis/graph"}));
    let pipeline = pipeline.with_interlocutor(human.clone());

    let plan: Plan = serde_json::from_value(json!([
        ask_step("E0", json!({"prompt": "Which repo?", "outputSchema": ASK_SCHEMA()})),
        {"id": "E1", "toolName": "t__search", "input": {"query": "{{E0.answer.repo}}"}},
    ]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(
        outcome.state.results["E0"]["answer"]["repo"],
        "tylerdavis/graph"
    );
    assert_eq!(outcome.state.results["E0"]["answered"], json!(true));
    // The whole point: a human's answer is ordinary step data.
    let calls = registry.invocations.lock().unwrap().clone();
    assert_eq!(calls[0].1["query"], "tylerdavis/graph");
}

#[tokio::test]
async fn the_question_renders_against_earlier_results() {
    let registry = search_registry(json!({"values": [{"id": "team-1"}]}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let human = ScriptedHuman::answering(json!({"repo": "x"}));
    let pipeline = pipeline.with_interlocutor(human.clone());

    let plan: Plan = serde_json::from_value(json!([
        {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
        ask_step(
            "E1",
            json!({"prompt": "Pick from {{E0.values}}", "outputSchema": ASK_SCHEMA()})
        ),
    ]))
    .unwrap();

    pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert!(
        human.prompts()[0].contains("team-1"),
        "{:?}",
        human.prompts()
    );
}

#[tokio::test]
async fn with_nobody_to_ask_the_default_declares_what_happens() {
    // The portability invariant: this is the CI run of an interactive plan.
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);

    let plan: Plan = serde_json::from_value(json!([ask_step(
        "E0",
        json!({
            "prompt": "Which repo?",
            "outputSchema": ASK_SCHEMA(),
            "whenUnanswered": "default",
            "default": {"repo": "tylerdavis/graph"}
        })
    )]))
    .unwrap();

    // No interlocutor installed at all — the headless shape.
    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(
        outcome.state.results["E0"]["answer"]["repo"],
        "tylerdavis/graph"
    );
    assert_eq!(outcome.state.results["E0"]["answered"], json!(false));
    assert_eq!(outcome.state.results["E0"]["reason"], "unavailable");
}

#[tokio::test]
async fn a_declining_human_is_distinguishable_from_an_absent_one() {
    // Both take the `default` path, but a plan can branch on which
    // happened — someone said no, versus nobody was there.
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let human = ScriptedHuman::new(vec![AskOutcome::Declined]);
    let pipeline = pipeline.with_interlocutor(human);

    let plan: Plan = serde_json::from_value(json!([ask_step(
        "E0",
        json!({
            "prompt": "Which repo?",
            "outputSchema": ASK_SCHEMA(),
            "whenUnanswered": "default",
            "default": {"repo": "fallback"}
        })
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E0"]["reason"], "declined");
}

#[tokio::test]
async fn the_default_is_rendered_not_taken_literally() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);

    let plan: Plan = serde_json::from_value(json!([ask_step(
        "E0",
        json!({
            "prompt": "Which repo?",
            "outputSchema": ASK_SCHEMA(),
            "whenUnanswered": "default",
            "default": {"repo": "{{input.repo}}"}
        })
    )]))
    .unwrap();

    let outcome = pipeline
        .run_explicit(
            "q",
            plan,
            Finish::Silent,
            Some(json!({"repo": "from-input"})),
        )
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E0"]["answer"]["repo"], "from-input");
}

#[tokio::test]
async fn an_unanswerable_ask_fails_by_default() {
    // `fail` is the default precisely so that an author who never thought
    // about the headless case finds out, instead of a plan silently
    // proceeding on a value nobody supplied.
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);

    let plan: Plan = serde_json::from_value(json!([ask_step(
        "E0",
        json!({"prompt": "Which repo?", "outputSchema": ASK_SCHEMA()})
    )]))
    .unwrap();

    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    match err {
        PipelineError::StepFailed { tool, message, .. } => {
            assert_eq!(tool, "ask");
            assert!(message.contains("whenUnanswered"), "{message}");
        }
        other => panic!("expected a step failure, got {other:?}"),
    }
}

#[tokio::test]
async fn an_answer_that_misses_the_schema_fails_rather_than_propagating() {
    // Hosts are not trusted to validate: a hand-typed TTY answer and a
    // partial client form both land here.
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let human = ScriptedHuman::answering(json!({"repo": 12}));
    let pipeline = pipeline.with_interlocutor(human);

    let plan: Plan = serde_json::from_value(json!([ask_step(
        "E0",
        json!({"prompt": "Which repo?", "outputSchema": ASK_SCHEMA()})
    )]))
    .unwrap();

    let err = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("does not conform"), "{text}");
}

#[tokio::test]
async fn an_ask_inside_a_map_body_sees_the_item() {
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry, 1);
    let human = ScriptedHuman::answering(json!({"repo": "picked"}));
    let pipeline = pipeline.with_interlocutor(human.clone());

    let plan: Plan = serde_json::from_value(json!([{
        "id": "E0",
        "toolName": "map",
        "input": {
            "over": ["alpha", "beta"],
            "do": {
                "toolName": "ask",
                "input": {"prompt": "Rename {{item}}?", "outputSchema": ASK_SCHEMA()}
            }
        }
    }]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.state.results["E0"]["count"], json!(2));
    assert_eq!(
        human.prompts(),
        vec!["Rename alpha?".to_string(), "Rename beta?".to_string()]
    );
    assert_eq!(
        outcome.state.results["E0"]["results"][0]["answer"]["repo"],
        "picked"
    );
}

#[tokio::test]
async fn an_exit_gate_can_branch_on_whether_a_human_answered() {
    // The composition that makes `ask` worth having as a step rather than
    // a special case: its result is ordinary data.
    let registry = search_registry(json!({}));
    let (pipeline, _) = pipeline(vec![], registry.clone(), 1);

    let plan: Plan = serde_json::from_value(json!([
        ask_step(
            "E0",
            json!({
                "prompt": "Approve?",
                "outputSchema": ASK_SCHEMA(),
                "whenUnanswered": "default",
                "default": {"repo": "none"}
            })
        ),
        {
            "id": "E1",
            "toolName": "exit",
            "input": {
                "when": {"value": "{{E0.answered}}", "op": "eq", "to": false},
                "status": "success",
                "message": "nobody approved — nothing to do"
            }
        },
        {"id": "E2", "toolName": "t__search", "input": {"query": "should not run"}},
    ]))
    .unwrap();

    let outcome = pipeline
        .run_explicit("q", plan, Finish::Silent, None)
        .await
        .unwrap();
    assert!(outcome.exit.is_some());
    assert!(registry.invocations.lock().unwrap().is_empty());
}

// ── Control-step discoverability ───────────────────────────────────────────

#[test]
fn every_control_step_is_described_in_the_catalog() {
    // The catalog is the only way an authoring agent can learn a control
    // step exists or what its input looks like. A step missing here sends
    // it to read graph's source instead — which is what happened before
    // `control_step_defs` was the single source of truth.
    let described: Vec<String> = control_step_defs().into_iter().map(|d| d.name).collect();
    for name in [
        AGENT_TOOL,
        ASK_TOOL,
        EXIT_TOOL,
        DECIDE_TOOL,
        FILTER_TOOL,
        MAP_TOOL,
        REDUCE_TOOL,
    ] {
        assert!(
            described.iter().any(|d| d == name),
            "control step '{name}' is not in the catalog: {described:?}"
        );
        assert!(
            is_control_step(name),
            "'{name}' is described as a control step but not recognised as one"
        );
    }
    assert_eq!(described.len(), 7, "{described:?}");
}

#[test]
fn a_control_step_is_never_mistaken_for_an_invokable_tool() {
    // `is_control_step` gates the "you cannot test this" refusal and the
    // listing's own group, so a false positive would hide a real tool.
    for name in [
        "plan_and_execute",
        "plan__report",
        "user__git_log",
        "t__search",
    ] {
        assert!(!is_control_step(name), "'{name}' is not a control step");
    }
}

#[test]
fn control_step_descriptions_carry_a_usable_input_schema() {
    // A description without a schema is not discovery — the agent still
    // has to guess the field names.
    for def in control_step_defs() {
        let schema = &def.input_schema;
        assert_eq!(
            schema["type"], "object",
            "{}: input schema is not an object",
            def.name
        );
        assert!(
            schema["properties"]
                .as_object()
                .is_some_and(|p| !p.is_empty()),
            "{}: input schema declares no properties",
            def.name
        );
        assert!(
            !def.description.trim().is_empty(),
            "{}: no description",
            def.name
        );
    }
}

#[test]
fn a_body_bearing_control_step_names_every_step_legal_in_its_body() {
    // These descriptions are prompt surface AND the catalog an agent
    // reads. When `ask` landed they still said "may contain an agent
    // step", which is how a planner learns a legal step is illegal.
    for def in control_step_defs() {
        if ![DECIDE_TOOL, MAP_TOOL, REDUCE_TOOL].contains(&def.name.as_str()) {
            continue;
        }
        for legal in [AGENT_TOOL, ASK_TOOL, FILTER_TOOL] {
            assert!(
                def.description.contains(&format!("`{legal}`")),
                "{}'s description does not mention that `{legal}` is legal in its body",
                def.name
            );
        }
    }
}
