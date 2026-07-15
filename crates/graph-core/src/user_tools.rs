//! User-defined tools: YAML documents loaded from `[tools].paths` and
//! registered under the `user__` namespace, visible to the agent, the
//! planner, and plan steps alike.
//!
//! Two kinds:
//! - `exec` — run a command; args/env are templated from the input.
//!   Arbitrary code execution by design; the user authors these.
//! - `prompt` — a templated LLM call, optionally with structured output.

use crate::template::{render_str, Roots};
use crate::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use graph_llm::types::{ChatMessage, ChatRequest, ResponseSchema};
use graph_llm::ModelRouter;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const USER_TOOL_PREFIX: &str = "user__";
pub const BUILTIN_TOOL_PREFIX: &str = "builtin__";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

// Note: no deny_unknown_fields — serde can't combine it with #[serde(flatten)].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToolDoc {
    /// Bare name; exposed as `user__<name>`.
    pub name: String,
    pub description: String,
    /// JSON Schema for the input (referenced as `{{input.x}}` in templates).
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// Declared output shape — feeds the planner's shape knowledge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(flatten)]
    pub kind: ToolKind,
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolKind {
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Environment for the process; values support `${VAR}` expansion
        /// from the parent environment at invoke time.
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// How to interpret stdout: `json` parses it, `text` wraps it.
        #[serde(default)]
        output: ExecOutput,
    },
    Prompt {
        /// Template rendered with the `input` root.
        prompt: String,
        /// Extra system prompt (optional).
        #[serde(default)]
        system: Option<String>,
        /// Which configured model runs the call: a role name (`chat`,
        /// `solver`, …) or a `[models.named]` entry. Default: chat.
        #[serde(default)]
        model: Option<String>,
        /// Take `output_schema` from the call's input instead of (only)
        /// the doc — the generic `builtin__infer` path, where each step
        /// decides between plain text and structured output.
        #[serde(default)]
        caller_output_schema: bool,
        /// Let the call's `model` input pick the model (overriding the
        /// doc's `model`) — the generic `builtin__infer` path. The
        /// catalog advertises configured `[models.named]` entries on the
        /// tool's input schema when this is set.
        #[serde(default)]
        caller_model: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutput {
    #[default]
    Json,
    Text,
}

// ── Bundled tool packs ───────────────────────────────────────────────────

/// Tool YAML shipped inside the binary, keyed by pack name. Pack tools load
/// through the same validation and registry machinery as user tools, but
/// are exposed under the `builtin__` namespace — copy one into a tools
/// directory (as a `user__` tool) to customize it.
const PACKS: &[(&str, &[&str])] = &[
    (
        "github",
        &[
            include_str!("packs/github/gh_pr_meta.yaml"),
            include_str!("packs/github/gh_pr_comment.yaml"),
            include_str!("packs/github/gh_pr_inline_comments.yaml"),
            include_str!("packs/github/gh_pr_ticket.yaml"),
            include_str!("packs/github/git_diff.yaml"),
            include_str!("packs/github/git_changed_files.yaml"),
            include_str!("packs/github/git_file.yaml"),
            include_str!("packs/github/git_grep.yaml"),
        ],
    ),
    ("llm", &[include_str!("packs/llm/infer.yaml")]),
];

/// Packs loaded whether or not `[tools].packs` names them — core
/// capabilities every catalog should have (currently just `llm`, whose
/// `builtin__infer` gives plans a generic string-or-structured LLM step).
pub const DEFAULT_PACKS: &[&str] = &["llm"];

pub fn available_packs() -> Vec<&'static str> {
    PACKS.iter().map(|(name, _)| *name).collect()
}

/// Parse the tools of the named packs. Unknown pack names error, listing
/// what exists — a typo should fail loudly at startup, not surface as
/// missing tools at plan time.
pub fn load_pack_tools(packs: &[String]) -> Result<Vec<UserToolDoc>, String> {
    let mut docs: Vec<UserToolDoc> = Vec::new();
    for pack in packs {
        let Some((_, sources)) = PACKS.iter().find(|(name, _)| name == pack) else {
            return Err(format!(
                "unknown tool pack '{pack}' (available: {})",
                available_packs().join(", ")
            ));
        };
        for raw in *sources {
            let doc: UserToolDoc =
                serde_yaml::from_str(raw).map_err(|e| format!("pack '{pack}': {e}"))?;
            validate_tool(&doc).map_err(|e| format!("pack '{pack}': {e}"))?;
            if docs.iter().any(|d| d.name == doc.name) {
                return Err(format!("pack '{pack}': duplicate tool name '{}'", doc.name));
            }
            docs.push(doc);
        }
    }
    Ok(docs)
}

// ── Loading & validation ─────────────────────────────────────────────────

pub fn load_user_tools(dirs: &[PathBuf]) -> Result<Vec<UserToolDoc>, String> {
    let mut docs: Vec<UserToolDoc> = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                )
            })
            .collect();
        entries.sort();
        for path in entries {
            let doc = load_user_tool(&path)?;
            if docs.iter().any(|d| d.name == doc.name) {
                return Err(format!("duplicate user tool name '{}'", doc.name));
            }
            docs.push(doc);
        }
    }
    Ok(docs)
}

pub fn load_user_tool(path: &Path) -> Result<UserToolDoc, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut doc: UserToolDoc =
        serde_yaml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    doc.path = Some(path.to_path_buf());
    validate_tool(&doc).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(doc)
}

pub fn validate_tool(doc: &UserToolDoc) -> Result<(), String> {
    if doc.name.is_empty()
        || !doc
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "tool name '{}' must be non-empty and use only [a-zA-Z0-9_-]",
            doc.name
        ));
    }
    let check_template = |template: &str, what: &str| -> Result<(), String> {
        let roots =
            crate::template::referenced_roots(template).map_err(|e| format!("{what}: {e}"))?;
        for root in roots {
            if root != "input" {
                return Err(format!(
                    "{what}: templates in user tools may only reference {{{{input.*}}}}, found {{{{{root}}}}}"
                ));
            }
        }
        Ok(())
    };
    match &doc.kind {
        ToolKind::Exec { command, args, .. } => {
            if command.is_empty() {
                return Err("exec tool needs a command".to_string());
            }
            for arg in args {
                check_template(arg, "args")?;
            }
        }
        ToolKind::Prompt { prompt, .. } => check_template(prompt, "prompt")?,
    }
    Ok(())
}

// ── Registry ─────────────────────────────────────────────────────────────

/// Registry over declarative tool documents. Serves user-authored tools
/// under `user__` and bundled pack tools under `builtin__` — same YAML
/// format, same validation, different namespace so the catalog says who
/// ships each tool.
pub struct UserToolRegistry {
    tools: Vec<UserToolDoc>,
    router: Arc<ModelRouter>,
    prefix: &'static str,
}

impl UserToolRegistry {
    pub fn new(tools: Vec<UserToolDoc>, router: Arc<ModelRouter>) -> Self {
        Self {
            tools,
            router,
            prefix: USER_TOOL_PREFIX,
        }
    }

    /// A registry serving bundled pack tools under `builtin__`.
    pub fn builtins(tools: Vec<UserToolDoc>, router: Arc<ModelRouter>) -> Self {
        Self {
            tools,
            router,
            prefix: BUILTIN_TOOL_PREFIX,
        }
    }

    async fn run(&self, doc: &UserToolDoc, mut input: Value) -> Result<ToolOutcome, ToolError> {
        if let Some(schema) = &doc.input_schema {
            crate::pipeline::doc::apply_schema_defaults(schema, &mut input);
        }
        if let Some(schema) = &doc.input_schema {
            if let Ok(validator) = jsonschema::validator_for(schema) {
                let problems: Vec<String> = validator
                    .iter_errors(&input)
                    .map(|e| e.to_string())
                    .collect();
                if !problems.is_empty() {
                    return Ok(ToolOutcome {
                        result: json!({"error": "invalid input", "problems": problems, "inputSchema": schema}),
                        is_error: true,
                    });
                }
            }
        }
        // A prompt tool that opted in takes its output schema (and model
        // name) from the call itself (the generic builtin__infer path) —
        // grab them before the input becomes template roots.
        let caller_schema = match &doc.kind {
            ToolKind::Prompt {
                caller_output_schema: true,
                ..
            } => input.get("output_schema").cloned(),
            _ => None,
        };
        let caller_model = match &doc.kind {
            ToolKind::Prompt {
                caller_model: true, ..
            } => input
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        let mut roots = Map::new();
        roots.insert("input".to_string(), input);
        let roots = Roots::new(&roots);

        match &doc.kind {
            ToolKind::Exec {
                command,
                args,
                env,
                cwd,
                timeout_secs,
                output,
            } => {
                self.run_exec(
                    command,
                    args,
                    env,
                    cwd.as_deref(),
                    *timeout_secs,
                    *output,
                    &roots,
                )
                .await
            }
            ToolKind::Prompt {
                prompt,
                system,
                model,
                ..
            } => {
                // Call-level model wins over the doc's pin.
                let model = caller_model.as_deref().or(model.as_deref());
                self.run_prompt(doc, prompt, system.as_deref(), model, caller_schema, &roots)
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_exec(
        &self,
        command: &str,
        args: &[String],
        env: &std::collections::BTreeMap<String, String>,
        cwd: Option<&Path>,
        timeout_secs: Option<u64>,
        output: ExecOutput,
        roots: &Roots<'_>,
    ) -> Result<ToolOutcome, ToolError> {
        let rendered_args: Vec<String> = args
            .iter()
            .map(|arg| render_str(arg, roots))
            .collect::<Result<_, _>>()
            .map_err(|e| ToolError::Transport(e.to_string()))?;

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&rendered_args);
        for (key, value) in env {
            let expanded = expand_env(value).map_err(ToolError::Transport)?;
            cmd.env(key, expanded);
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.kill_on_drop(true);

        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let result = tokio::time::timeout(timeout, cmd.output()).await;
        let out = match result {
            Err(_) => {
                return Ok(ToolOutcome {
                    result: json!({"error": format!("command timed out after {}s", timeout.as_secs())}),
                    is_error: true,
                })
            }
            Ok(Err(e)) => {
                return Ok(ToolOutcome {
                    result: json!({"error": format!("failed to run '{command}': {e}")}),
                    is_error: true,
                })
            }
            Ok(Ok(out)) => out,
        };

        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            return Ok(ToolOutcome {
                result: json!({
                    "error": format!("'{command}' exited with {}", out.status),
                    "stderr": stderr,
                    "stdout": stdout,
                }),
                is_error: true,
            });
        }
        match output {
            ExecOutput::Json => match serde_json::from_str(&stdout) {
                Ok(parsed) => Ok(ToolOutcome {
                    result: parsed,
                    is_error: false,
                }),
                Err(e) => Ok(ToolOutcome {
                    result: json!({
                        "error": format!("stdout is not valid JSON: {e}"),
                        "stdout": stdout,
                    }),
                    is_error: true,
                }),
            },
            ExecOutput::Text => Ok(ToolOutcome {
                result: json!({"text": stdout}),
                is_error: false,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Add a `model` property to a caller-model prompt tool's catalog
    /// schema, enumerating the configured `[models.named]` entries with
    /// their descriptions — the planner's routing signal for picking the
    /// smallest adequate model. No named models configured → no property:
    /// a knob with nothing to select would only invite invented names.
    fn advertise_named_models(&self, schema: &mut Value) {
        let named = self.router.named_models();
        if named.is_empty() {
            return;
        }
        let names: Vec<&String> = named.keys().collect();
        let catalog = named
            .iter()
            .map(|(name, choice)| match &choice.description {
                Some(description) => format!("{name} — {description}"),
                None => format!("{name} — {}", choice.model),
            })
            .collect::<Vec<_>>()
            .join("; ");
        let description = format!(
            "Which configured model runs this inference. Available: {catalog}. \
             Prefer the smallest adequate model for small, self-contained \
             instructions (for example per-item map bodies); omit to use \
             the default."
        );
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert(
                "model".to_string(),
                json!({"type": "string", "enum": names, "description": description}),
            );
        }
    }

    async fn run_prompt(
        &self,
        doc: &UserToolDoc,
        prompt: &str,
        system: Option<&str>,
        model: Option<&str>,
        caller_schema: Option<Value>,
        roots: &Roots<'_>,
    ) -> Result<ToolOutcome, ToolError> {
        // The doc's schema wins; a caller-supplied one applies only when
        // the tool opted in (see `caller_output_schema`).
        let schema = doc.output_schema.as_ref().or(caller_schema.as_ref());
        let rendered =
            render_str(prompt, roots).map_err(|e| ToolError::Transport(e.to_string()))?;
        let request = ChatRequest {
            system: system.unwrap_or_default().to_string(),
            messages: vec![ChatMessage::User { content: rendered }],
            response_schema: schema.map(|schema| ResponseSchema {
                name: doc.name.clone(),
                schema: schema.clone(),
            }),
            ..Default::default()
        };
        let response = self
            .router
            .chat_named(model.unwrap_or("chat"), request)
            .await
            .map_err(|e| ToolError::Transport(e.to_string()))?;
        let result = match response.structured {
            Some(structured) => structured,
            None => json!({"text": response.content.unwrap_or_default()}),
        };
        // Provider-native structured output isn't schema-*validated* by the
        // provider — models can omit required fields. With a declared
        // output_schema, enforce it here with one repair pass, so plans can
        // rely on every declared field existing (a missing key is a hard
        // template error downstream).
        let result = match schema {
            Some(schema) => self.enforce_schema(result, schema).await?,
            None => result,
        };
        Ok(ToolOutcome {
            result,
            is_error: false,
        })
    }

    /// Validate `value` against `schema`; on mismatch, run one repair pass
    /// and re-validate. A value that still doesn't conform is a tool error —
    /// better a failed step than a silently missing field.
    async fn enforce_schema(&self, value: Value, schema: &Value) -> Result<Value, ToolError> {
        let validator = jsonschema::validator_for(schema)
            .map_err(|e| ToolError::Transport(format!("invalid output_schema: {e}")))?;
        let problems = |value: &Value| -> Option<String> {
            let errors: Vec<String> = validator
                .iter_errors(value)
                .map(|e| e.to_string())
                .collect();
            (!errors.is_empty()).then(|| errors.join("; "))
        };
        let Some(error) = problems(&value) else {
            return Ok(value);
        };
        let repaired = self
            .router
            .repair_structured(&value, schema, &error)
            .await
            .map_err(|e| ToolError::Transport(format!("output repair failed: {e}")))?;
        match problems(&repaired) {
            None => Ok(repaired),
            Some(still) => Err(ToolError::Transport(format!(
                "output does not match output_schema after repair: {still}"
            ))),
        }
    }
}

fn expand_env(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated ${{...}} in env value: {value:?}"))?;
        let var = &after[..end];
        let resolved = std::env::var(var)
            .map_err(|_| format!("environment variable {var} referenced by tool env is not set"))?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[async_trait]
impl ToolRegistry for UserToolRegistry {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(self
            .tools
            .iter()
            .map(|doc| {
                let mut input_schema = doc
                    .input_schema
                    .clone()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                if matches!(
                    &doc.kind,
                    ToolKind::Prompt {
                        caller_model: true,
                        ..
                    }
                ) {
                    self.advertise_named_models(&mut input_schema);
                }
                ToolDef {
                    name: format!("{}{}", self.prefix, doc.name),
                    description: doc.description.clone(),
                    input_schema,
                    output_schema: doc.output_schema.clone(),
                    output_example: None,
                    read_only: doc.read_only.or(match &doc.kind {
                        ToolKind::Prompt { .. } => Some(true),
                        ToolKind::Exec { .. } => None,
                    }),
                }
            })
            .collect())
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let Some(bare) = name.strip_prefix(self.prefix) else {
            return Err(ToolError::Unknown(name.to_string()));
        };
        let Some(doc) = self.tools.iter().find(|d| d.name == bare) else {
            return Err(ToolError::Unknown(name.to_string()));
        };
        self.run(doc, input).await
    }
}
