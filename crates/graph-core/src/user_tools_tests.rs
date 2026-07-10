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

// ── Bundled tool packs ───────────────────────────────────────────────────

#[test]
fn github_pack_loads_and_validates() {
    let docs = load_pack_tools(&["github".to_string()]).unwrap();
    let names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "gh_pr_meta",
            "gh_pr_comment",
            "git_diff",
            "git_changed_files"
        ]
    );
    // gh_pr_comment posts; everything else is read-only.
    for doc in &docs {
        let read_only = doc.read_only.unwrap_or(false);
        assert_eq!(read_only, doc.name != "gh_pr_comment", "{}", doc.name);
    }
}

#[test]
fn unknown_pack_errors_with_available_list() {
    let err = load_pack_tools(&["gitlab".to_string()]).unwrap_err();
    assert!(err.contains("gitlab"), "{err}");
    assert!(err.contains("github"), "{err}");
}

#[test]
fn user_tool_shadows_pack_tool() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("git_diff.yaml"),
        r#"
name: git_diff
description: local override
kind: exec
command: echo
"#,
    )
    .unwrap();
    let docs = load_tools_with_packs(&["github".to_string()], &[dir.path().to_path_buf()]).unwrap();
    let overridden = docs.iter().find(|d| d.name == "git_diff").unwrap();
    assert_eq!(overridden.description, "local override");
    assert_eq!(docs.iter().filter(|d| d.name == "git_diff").count(), 1);
    // The rest of the pack is still present.
    assert!(docs.iter().any(|d| d.name == "gh_pr_meta"));
}

// ── Prompt-tool output_schema enforcement ────────────────────────────────

/// Router whose chat role returns `chat_value` and repair role `repair_value`.
fn schema_router(chat_value: Value, repair_value: Value) -> Arc<ModelRouter> {
    struct Fixed(Value);
    #[async_trait]
    impl ChatProvider for Fixed {
        async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                content: None,
                tool_calls: vec![],
                structured: Some(self.0.clone()),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("chat".into(), Arc::new(Fixed(chat_value)));
    providers.insert("fixer".into(), Arc::new(Fixed(repair_value)));
    let choice = |provider: &str| ModelChoice {
        provider: provider.into(),
        model: "m".into(),
        temperature: None,
        dimensions: None,
    };
    Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles {
            default: Some(choice("chat")),
            repair: Some(choice("fixer")),
            ..Default::default()
        },
    ))
}

fn strict_prompt_tool() -> UserToolDoc {
    doc(r#"
name: strict
description: prompt tool with a required output field
kind: prompt
prompt: "judge {{input.x}}"
output_schema:
  type: object
  required: [category, severity]
  properties:
    category: { type: string }
    severity: { type: string }
"#)
}

#[tokio::test]
async fn prompt_output_missing_field_is_repaired() {
    let router = schema_router(
        json!({"category": "bug"}),                    // invalid: no severity
        json!({"category": "bug", "severity": "low"}), // repair fixes it
    );
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router, None);
    let outcome = registry
        .invoke("user__strict", json!({"x": 1}))
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result["severity"], "low");
}

#[tokio::test]
async fn prompt_output_unrepairable_is_an_error() {
    let router = schema_router(
        json!({"category": "bug"}), // invalid
        json!({"category": "bug"}), // repair returns the same invalid doc
    );
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router, None);
    let err = registry
        .invoke("user__strict", json!({"x": 1}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("after repair"), "{err}");
}

#[tokio::test]
async fn prompt_output_valid_passes_untouched() {
    let router = schema_router(
        json!({"category": "bug", "severity": "high"}), // already valid
        json!({"category": "WRONG", "severity": "WRONG"}), // repair must not run
    );
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router, None);
    let outcome = registry
        .invoke("user__strict", json!({"x": 1}))
        .await
        .unwrap();
    assert_eq!(outcome.result["severity"], "high");
}
