//! Tests for user-defined tools.

use crate::user_tools::*;
use crate::{ToolError, ToolRegistry};
use async_trait::async_trait;
use graph_config::{ModelChoice, ModelRoles};
use graph_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, Usage};
use graph_llm::{ChatProvider, LlmError, ModelRouter};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn router() -> Arc<ModelRouter> {
    struct Canned;
    #[async_trait]
    impl ChatProvider for Canned {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            let structured = req
                .response_schema
                .as_ref()
                .map(|_| json!({"category": "bug"}));
            Ok(ChatResponse {
                content: Some(format!(
                    "echo: {}",
                    match &req.messages[0] {
                        graph_llm::types::ChatMessage::User { content } => content.clone(),
                        _ => String::new(),
                    }
                )),
                tool_calls: vec![],
                structured,
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), Arc::new(Canned));
    Arc::new(ModelRouter::with_providers(
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
    ))
}

fn doc(yaml: &str) -> UserToolDoc {
    let doc: UserToolDoc = serde_yaml::from_str(yaml).unwrap();
    validate_tool(&doc).unwrap();
    doc
}

fn registry(docs: Vec<UserToolDoc>, cypher: Option<Arc<dyn CypherExecutor>>) -> UserToolRegistry {
    UserToolRegistry::new(docs, router(), cypher)
}

#[tokio::test]
async fn exec_tool_renders_args_and_parses_json() {
    let tool = doc(r#"
name: emit
description: emit json
kind: exec
command: echo
args: ['{"got": "{{input.word}}"}']
input_schema:
  type: object
  required: [word]
  properties:
    word: { type: string }
"#);
    let registry = registry(vec![tool], None);
    let outcome = registry
        .invoke("user__emit", json!({"word": "hello"}))
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result, json!({"got": "hello"}));
}

#[tokio::test]
async fn exec_tool_text_mode_and_failure() {
    let text_tool = doc(r#"
name: say
description: say text
kind: exec
command: echo
args: ["plain output"]
output: text
"#);
    let failing = doc(r#"
name: fail
description: exits nonzero
kind: exec
command: sh
args: ["-c", "echo oops >&2; exit 3"]
"#);
    let registry = registry(vec![text_tool, failing], None);

    let outcome = registry.invoke("user__say", json!({})).await.unwrap();
    assert_eq!(outcome.result, json!({"text": "plain output"}));

    let outcome = registry.invoke("user__fail", json!({})).await.unwrap();
    assert!(outcome.is_error);
    assert_eq!(outcome.result["stderr"], "oops");
}

#[tokio::test]
async fn exec_tool_missing_input_is_a_schema_error() {
    let tool = doc(r#"
name: emit
description: emit json
kind: exec
command: echo
args: ['{{input.word}}']
input_schema:
  type: object
  required: [word]
  properties:
    word: { type: string }
"#);
    let registry = registry(vec![tool], None);
    let outcome = registry.invoke("user__emit", json!({})).await.unwrap();
    assert!(outcome.is_error);
    assert!(outcome.result["problems"][0]
        .as_str()
        .unwrap()
        .contains("word"));
}

#[tokio::test]
async fn prompt_tool_returns_structured_output_when_schema_given() {
    let tool = doc(r#"
name: classify
description: classify text
kind: prompt
prompt: "Classify: {{input.text}}"
output_schema:
  type: object
  properties:
    category: { type: string }
"#);
    let registry = registry(vec![tool], None);
    let outcome = registry
        .invoke("user__classify", json!({"text": "login is broken"}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"category": "bug"}));
}

#[tokio::test]
async fn cypher_tool_binds_input_params() {
    type SeenQuery = (String, Vec<(String, Value)>);
    struct FakeCypher {
        seen: std::sync::Mutex<Vec<SeenQuery>>,
    }
    #[async_trait]
    impl CypherExecutor for FakeCypher {
        async fn query(
            &self,
            cypher: &str,
            params: Vec<(String, Value)>,
        ) -> Result<Vec<Map<String, Value>>, ToolError> {
            self.seen.lock().unwrap().push((cypher.to_string(), params));
            let mut row = Map::new();
            row.insert("n".into(), json!(7));
            Ok(vec![row])
        }
    }
    let fake = Arc::new(FakeCypher {
        seen: Default::default(),
    });
    let tool = doc(r#"
name: count_shapes
description: count tool shapes
kind: cypher
query: "MATCH (s:ToolShape) WHERE s.seen_count >= $min RETURN count(s) AS n"
"#);
    let registry = registry(vec![tool], Some(fake.clone()));
    let outcome = registry
        .invoke("user__count_shapes", json!({"min": 2}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"rows": [{"n": 7}], "count": 1}));
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen[0].1, vec![("min".to_string(), json!(2))]);
}

#[tokio::test]
async fn cypher_without_backend_errors_cleanly() {
    let tool = doc(r#"
name: q
description: query
kind: cypher
query: "MATCH (n) RETURN n"
"#);
    let registry = registry(vec![tool], None);
    let outcome = registry.invoke("user__q", json!({})).await.unwrap();
    assert!(outcome.is_error);
    assert!(outcome.result["error"]
        .as_str()
        .unwrap()
        .contains("ladybug"));
}

#[test]
fn validation_rejects_non_input_roots_and_bad_names() {
    let bad_root: UserToolDoc = serde_yaml::from_str(
        r#"
name: bad
description: x
kind: exec
command: echo
args: ["{{E0.values}}"]
"#,
    )
    .unwrap();
    assert!(validate_tool(&bad_root).unwrap_err().contains("E0"));

    let bad_name: UserToolDoc = serde_yaml::from_str(
        r#"
name: "no spaces"
description: x
kind: exec
command: echo
"#,
    )
    .unwrap();
    assert!(validate_tool(&bad_name).is_err());
}
