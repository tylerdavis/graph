//! Tests for user-defined tools.

use crate::user_tools::*;
use crate::{ToolError, ToolRegistry};
use async_trait::async_trait;
use graph_config::{ModelChoice, ModelRoles};
use graph_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, Usage};
use graph_llm::{ChatProvider, LlmError, ModelRouter};
use serde_json::{json, Value};
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
                description: None,
                fallbacks: Vec::new(),
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

fn registry(docs: Vec<UserToolDoc>) -> UserToolRegistry {
    UserToolRegistry::new(docs, router())
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
    let registry = registry(vec![tool]);
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
    let registry = registry(vec![text_tool, failing]);

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
    let registry = registry(vec![tool]);
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
    let registry = registry(vec![tool]);
    let outcome = registry
        .invoke("user__classify", json!({"text": "login is broken"}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"category": "bug"}));
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
            "gh_pr_inline_comments",
            "gh_pr_ticket",
            "git_diff",
            "git_changed_files",
            "git_file",
            "git_grep"
        ]
    );
    // The comment tools post; everything else is read-only.
    for doc in &docs {
        let read_only = doc.read_only.unwrap_or(false);
        let posts = matches!(doc.name.as_str(), "gh_pr_comment" | "gh_pr_inline_comments");
        assert_eq!(read_only, !posts, "{}", doc.name);
    }
}

#[test]
fn gh_pr_ticket_default_pattern_requires_a_separator() {
    // Regression: the original default `[A-Za-z]{2,6}[- ]?[0-9]+` matched
    // glued technical words (arm64, utf8, sha256), so ticket-free PRs came
    // back `found: true` with a bogus ID. The separator between letters and
    // digits must be mandatory, not optional, so those words don't match.
    let docs = load_pack_tools(&["github".to_string()]).unwrap();
    let ticket = docs.iter().find(|d| d.name == "gh_pr_ticket").unwrap();
    let default = ticket.input_schema.as_ref().unwrap()["properties"]["pattern"]["default"]
        .as_str()
        .unwrap();
    assert_eq!(default, r"\b[A-Za-z]{2,6}[- ][0-9]+\b");
    assert!(
        !default.contains("[- ]?"),
        "default pattern must require a separator, not make it optional: {default}"
    );
}

#[test]
fn unknown_pack_errors_with_available_list() {
    let err = load_pack_tools(&["gitlab".to_string()]).unwrap_err();
    assert!(err.contains("gitlab"), "{err}");
    assert!(err.contains("github"), "{err}");
}

#[tokio::test]
async fn pack_tools_serve_under_builtin_namespace() {
    let docs = load_pack_tools(&["github".to_string()]).unwrap();
    let registry = UserToolRegistry::builtins(docs, router());

    let names: Vec<String> = registry
        .tools()
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(
        names.iter().all(|n| n.starts_with("builtin__")),
        "{names:?}"
    );
    assert!(names.contains(&"builtin__gh_pr_meta".to_string()));

    // Invocation honors the namespace: builtin__ resolves, user__ doesn't.
    let err = registry
        .invoke("user__gh_pr_meta", json!({"pr": 1}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Unknown(_)));
}

#[tokio::test]
async fn llm_pack_infer_returns_text_or_caller_structured_output() {
    let docs = load_pack_tools(&["llm".to_string()]).unwrap();
    assert_eq!(docs.len(), 1);
    let registry = UserToolRegistry::builtins(docs, router());

    // No output_schema in the call: plain text.
    let outcome = registry
        .invoke("builtin__infer", json!({"instruction": "Summarize X"}))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!({"text": "echo: Summarize X"}));

    // With an output_schema: structured, validated JSON.
    let outcome = registry
        .invoke(
            "builtin__infer",
            json!({
                "instruction": "Classify X",
                "output_schema": {
                    "type": "object",
                    "properties": {"category": {"type": "string"}},
                    "required": ["category"],
                },
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!({"category": "bug"}));
}

// ── Reshape (data pack) ──────────────────────────────────────────────────

#[tokio::test]
async fn data_pack_reshape_projects_a_new_shape() {
    let docs = load_pack_tools(&["data".to_string()]).unwrap();
    assert_eq!(docs.len(), 1);
    let registry = UserToolRegistry::builtins(docs, router());

    // Standalone (no plan-level render), the shape's leaves reference the
    // tool's own `input` root. Rename keys, splice typed values, interpolate.
    let outcome = registry
        .invoke(
            "builtin__reshape",
            json!({
                "baseRefOid": "abc",
                "number": 12,
                "labels": ["bug"],
                "shape": {
                    "base_sha": "{{input.baseRefOid}}",
                    "pr": "{{input.number}}",
                    "tags": "{{input.labels}}",
                    "title": "PR #{{input.number}}",
                },
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({
            "base_sha": "abc",
            "pr": 12,            // exact tag keeps the number type
            "tags": ["bug"],     // exact tag keeps the array type
            "title": "PR #12",   // mixed text renders to a string
        })
    );
}

#[tokio::test]
async fn reshape_already_rendered_shape_passes_through() {
    // Inside a plan the pipeline renders the step input first, so the shape
    // reaches the tool as concrete literals (no templates left). The tool's
    // render is then a no-op that returns the object verbatim.
    let docs = load_pack_tools(&["data".to_string()]).unwrap();
    let registry = UserToolRegistry::builtins(docs, router());
    let outcome = registry
        .invoke(
            "builtin__reshape",
            json!({"shape": {"base_sha": "abc", "pr": 12, "tags": ["bug"]}}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({"base_sha": "abc", "pr": 12, "tags": ["bug"]})
    );
}

#[tokio::test]
async fn reshape_read_only_and_bad_path_is_a_tool_error() {
    let docs = load_pack_tools(&["data".to_string()]).unwrap();
    let registry = UserToolRegistry::builtins(docs, router());

    // Pure transform: advertised read-only.
    let read_only = registry
        .tools()
        .await
        .unwrap()
        .iter()
        .find(|d| d.name == "builtin__reshape")
        .unwrap()
        .read_only;
    assert_eq!(read_only, Some(true));

    // A shape referencing a field the input doesn't have is a tool error,
    // not a panic — the same contract as an exec that exits non-zero.
    let outcome = registry
        .invoke(
            "builtin__reshape",
            json!({
                "id": 1,
                "shape": {"missing": "{{input.nope}}"},
            }),
        )
        .await
        .unwrap();
    assert!(outcome.is_error, "{:?}", outcome.result);
    assert!(outcome.result["error"]
        .as_str()
        .unwrap()
        .contains("reshape failed"));
}

#[test]
fn reshape_validation_needs_a_shape_and_rejects_non_input_roots() {
    // A fixed-shape reshape tool with no shape and no caller_shape is invalid.
    let no_shape: UserToolDoc = serde_yaml::from_str(
        r#"
name: bad
description: x
kind: reshape
"#,
    )
    .unwrap();
    assert!(validate_tool(&no_shape)
        .unwrap_err()
        .contains("caller_shape"));

    // Fixed-shape leaves are validated at load time: only {{input.*}}.
    let bad_root: UserToolDoc = serde_yaml::from_str(
        r#"
name: bad
description: x
kind: reshape
shape:
  x: "{{E0.value}}"
"#,
    )
    .unwrap();
    assert!(validate_tool(&bad_root).unwrap_err().contains("E0"));

    // A fixed shape that only references input passes.
    let ok = doc(r#"
name: fixed
description: fixed projection
kind: reshape
shape:
  sha: "{{input.baseRefOid}}"
"#);
    let registry = registry(vec![ok]);
    let _ = &registry;
}

#[tokio::test]
async fn reshape_fixed_shape_renders_against_input() {
    let tool = doc(r#"
name: fixed
description: fixed projection
kind: reshape
shape:
  sha: "{{input.baseRefOid}}"
  n: "{{input.number}}"
"#);
    let registry = registry(vec![tool]);
    let outcome = registry
        .invoke("user__fixed", json!({"baseRefOid": "abc", "number": 7}))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!({"sha": "abc", "n": 7}));
}

#[tokio::test]
async fn caller_output_schema_requires_opt_in() {
    // A plain prompt tool ignores output_schema in the input — the schema
    // surface is the doc's unless the tool sets caller_output_schema.
    let tool = doc(r#"
name: summarize
description: summarize text
kind: prompt
prompt: "Summarize: {{input.text}}"
"#);
    let registry = registry(vec![tool]);
    let outcome = registry
        .invoke(
            "user__summarize",
            json!({"text": "hi", "output_schema": {"type": "object"}}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result, json!({"text": "echo: Summarize: hi"}));
}

#[tokio::test]
async fn caller_schema_without_type_gets_object_defaulted() {
    // A caller who authored `output_schema` as just `{properties, required}`
    // (no top-level `type`) must still get valid structured output — the
    // provider is handed a schema with `"type": "object"` filled in, not the
    // invalid one that would 400. Capture what reaches the provider.
    use std::sync::Mutex;
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Option<Value>>>);
    #[async_trait]
    impl ChatProvider for Capture {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            *self.0.lock().unwrap() = req.response_schema.as_ref().map(|s| s.schema.clone());
            Ok(ChatResponse {
                content: None,
                tool_calls: vec![],
                structured: Some(json!({"pattern": "x", "reason": "y"})),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }
    let seen = Arc::new(Mutex::new(None));
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), Arc::new(Capture(seen.clone())));
    let router = Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles {
            default: Some(ModelChoice {
                provider: "mock".into(),
                model: "m".into(),
                temperature: None,
                dimensions: None,
                description: None,
                fallbacks: Vec::new(),
            }),
            ..Default::default()
        },
    ));
    let tool = doc(r#"
name: infer
description: generic inference
kind: prompt
prompt: "{{input.instruction}}"
caller_output_schema: true
input_schema:
  type: object
  required: [instruction]
  properties:
    instruction: { type: string }
    output_schema: { type: object }
"#);
    let registry = UserToolRegistry::new(vec![tool], router);
    let outcome = registry
        .invoke(
            "user__infer",
            json!({
                "instruction": "classify",
                // No top-level `type` — exactly the shape that 400'd in the wild.
                "output_schema": {
                    "properties": {"pattern": {"type": "string"}, "reason": {"type": "string"}},
                    "required": ["pattern", "reason"],
                },
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "result: {:?}", outcome.result);
    let forwarded = seen.lock().unwrap().clone().expect("schema forwarded");
    assert_eq!(
        forwarded["type"], "object",
        "top-level type must be defaulted before reaching the provider"
    );
}

// ── Git-backed pack tools (real git, scratch repo, no network) ───────────

/// One-commit git repo: greeting.txt at the root, src/lib.rs with two fns.
fn scratch_git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(
        dir.path().join("greeting.txt"),
        "hello graph\nsecond line\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "fn alpha() {}\nfn beta() {}\n",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "init"]);
    dir
}

/// The named github pack tool, pinned to run inside `cwd`.
fn github_tool_in(name: &str, cwd: &std::path::Path) -> UserToolDoc {
    let mut doc = load_pack_tools(&["github".to_string()])
        .unwrap()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no pack tool '{name}'"));
    if let ToolKind::Exec { cwd: dir, .. } = &mut doc.kind {
        *dir = Some(cwd.to_path_buf());
    }
    doc
}

#[tokio::test]
async fn git_file_reads_content_at_ref_and_marks_truncation() {
    let repo = scratch_git_repo();
    let registry =
        UserToolRegistry::builtins(vec![github_tool_in("git_file", repo.path())], router());

    // max_bytes defaulted: full content.
    let outcome = registry
        .invoke(
            "builtin__git_file",
            json!({"ref": "HEAD", "path": "greeting.txt"}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!({"text": "hello graph\nsecond line"}));

    // Over budget: capped, with the truncation marker line.
    let outcome = registry
        .invoke(
            "builtin__git_file",
            json!({"ref": "HEAD", "path": "greeting.txt", "max_bytes": 5}),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.result,
        json!({"text": "hello\n\n[file truncated: showing 5 of 24 bytes]"})
    );

    // A missing path fails loudly, not as empty content.
    let outcome = registry
        .invoke(
            "builtin__git_file",
            json!({"ref": "HEAD", "path": "missing.txt"}),
        )
        .await
        .unwrap();
    assert!(outcome.is_error);
}

#[tokio::test]
async fn git_grep_returns_structured_matches_with_defaults() {
    let repo = scratch_git_repo();
    let registry =
        UserToolRegistry::builtins(vec![github_tool_in("git_grep", repo.path())], router());

    // ref/paths/max_matches all defaulted (HEAD, everything, 200).
    let outcome = registry
        .invoke("builtin__git_grep", json!({"pattern": "fn (alpha|beta)"}))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({
            "matches": [
                {"path": "src/lib.rs", "line": 1, "text": "fn alpha() {}"},
                {"path": "src/lib.rs", "line": 2, "text": "fn beta() {}"},
            ],
            "count": 2,
            "truncated": false,
        })
    );

    // Pathspec scoping + cap: one match returned, the cut flagged.
    let outcome = registry
        .invoke(
            "builtin__git_grep",
            json!({"pattern": "fn", "paths": "src/", "max_matches": 1}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.result["count"], 1);
    assert_eq!(outcome.result["truncated"], true);

    // No matches is a result, not an error.
    let outcome = registry
        .invoke(
            "builtin__git_grep",
            json!({"pattern": "nowhere_to_be_found"}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({"matches": [], "count": 0, "truncated": false})
    );

    // A bad ref fails loudly instead of reading as "no matches".
    let outcome = registry
        .invoke(
            "builtin__git_grep",
            json!({"pattern": "fn", "ref": "no-such-ref"}),
        )
        .await
        .unwrap();
    assert!(outcome.is_error);
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
        description: None,
        fallbacks: Vec::new(),
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
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router);
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
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router);
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
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router);
    let outcome = registry
        .invoke("user__strict", json!({"x": 1}))
        .await
        .unwrap();
    assert_eq!(outcome.result["severity"], "high");
}

// ── Named models ─────────────────────────────────────────────────────────

/// Router that echoes the resolved model id back in the response text,
/// with two `[models.named]` entries alongside the default.
fn named_model_router() -> Arc<ModelRouter> {
    struct Echo;
    #[async_trait]
    impl ChatProvider for Echo {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                content: Some(format!("model={}", req.model)),
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
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), Arc::new(Echo));
    let choice = |model: &str, description: Option<&str>| ModelChoice {
        provider: "mock".into(),
        model: model.into(),
        temperature: None,
        dimensions: None,
        description: description.map(str::to_string),
        fallbacks: Vec::new(),
    };
    let mut named = std::collections::BTreeMap::new();
    named.insert(
        "nano".to_string(),
        choice("nano-model", Some("fast and cheap")),
    );
    named.insert("deep".to_string(), choice("deep-model", None));
    Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles {
            default: Some(choice("default-model", None)),
            named,
            ..Default::default()
        },
    ))
}

#[tokio::test]
async fn prompt_tool_model_pin_routes_to_named_model() {
    let tool = doc(r#"
name: pinned
description: runs on a named model
kind: prompt
prompt: "go"
model: nano
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    let outcome = registry.invoke("user__pinned", json!({})).await.unwrap();
    assert_eq!(outcome.result, json!({"text": "model=nano-model"}));
}

#[tokio::test]
async fn caller_model_overrides_the_doc_pin() {
    let tool = doc(r#"
name: pick
description: caller picks the model
kind: prompt
prompt: "go"
model: deep
caller_model: true
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    let outcome = registry
        .invoke("user__pick", json!({"model": "nano"}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"text": "model=nano-model"}));
    let outcome = registry.invoke("user__pick", json!({})).await.unwrap();
    assert_eq!(outcome.result, json!({"text": "model=deep-model"}));
}

#[tokio::test]
async fn unknown_model_name_errors_listing_configured_names() {
    let tool = doc(r#"
name: broken
description: pins a model that is not configured
kind: prompt
prompt: "go"
model: bogus
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    let err = registry
        .invoke("user__broken", json!({}))
        .await
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("no model named 'bogus'"), "{message}");
    assert!(message.contains("nano"), "{message}");
}

#[tokio::test]
async fn role_names_still_resolve_with_default_fallback() {
    let tool = doc(r#"
name: solver_tool
description: runs under the solver role
kind: prompt
prompt: "go"
model: solver
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    // No solver entry configured: falls back to default.
    let outcome = registry
        .invoke("user__solver_tool", json!({}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"text": "model=default-model"}));
}

#[tokio::test]
async fn caller_model_tools_advertise_named_models_in_the_catalog() {
    let docs = load_pack_tools(&["llm".to_string()]).unwrap();

    // Named models configured: the catalog schema advertises them.
    let registry = UserToolRegistry::builtins(docs.clone(), named_model_router());
    let defs = registry.tools().await.unwrap();
    let infer = defs.iter().find(|d| d.name == "builtin__infer").unwrap();
    let model = &infer.input_schema["properties"]["model"];
    assert_eq!(model["enum"], json!(["deep", "nano"]));
    let description = model["description"].as_str().unwrap();
    assert!(
        description.contains("nano — fast and cheap"),
        "{description}"
    );
    assert!(description.contains("deep — deep-model"), "{description}");

    // None configured: no model property — nothing to select.
    let registry = UserToolRegistry::builtins(docs, router());
    let defs = registry.tools().await.unwrap();
    let infer = defs.iter().find(|d| d.name == "builtin__infer").unwrap();
    assert!(infer.input_schema["properties"].get("model").is_none());
}

#[tokio::test]
async fn llm_pack_infer_routes_by_model_input() {
    let docs = load_pack_tools(&["llm".to_string()]).unwrap();
    let registry = UserToolRegistry::builtins(docs, named_model_router());
    let outcome = registry
        .invoke(
            "builtin__infer",
            json!({"instruction": "go", "model": "nano"}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"text": "model=nano-model"}));
}
