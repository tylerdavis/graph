//! User-defined tools: YAML documents loaded from `[tools].paths` and
//! registered under the `user__` namespace, visible to the agent, the
//! planner, and plan steps alike.
//!
//! Three kinds:
//! - `exec` — run a command; args/env are templated from the input.
//!   Arbitrary code execution by design; the user authors these.
//! - `prompt` — a templated LLM call, optionally with structured output.
//! - `cypher` — a parameterized read-only query against the embedded
//!   database (requires the ladybug storage backend).

use crate::template::{render_str, Roots};
use crate::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use graph_config::Role;
use graph_llm::types::{ChatMessage, ChatRequest, ResponseSchema};
use graph_llm::ModelRouter;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const USER_TOOL_PREFIX: &str = "user__";
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
        /// Model role to run under (default: chat).
        #[serde(default)]
        role: PromptRole,
    },
    Cypher {
        /// Query with `$name` parameters bound from the input object.
        query: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutput {
    #[default]
    Json,
    Text,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptRole {
    #[default]
    Chat,
    Planner,
    Solver,
    Repair,
}

impl From<PromptRole> for Role {
    fn from(role: PromptRole) -> Self {
        match role {
            PromptRole::Chat => Role::Chat,
            PromptRole::Planner => Role::Planner,
            PromptRole::Solver => Role::Solver,
            PromptRole::Repair => Role::Repair,
        }
    }
}

/// Read-only Cypher access to the embedded database, implemented by the
/// ladybug store. `None` on other backends.
#[async_trait]
pub trait CypherExecutor: Send + Sync {
    /// Rows as objects keyed by column name.
    async fn query(
        &self,
        cypher: &str,
        params: Vec<(String, Value)>,
    ) -> Result<Vec<Map<String, Value>>, ToolError>;
}

// ── Bundled tool packs ───────────────────────────────────────────────────

/// Tool YAML shipped inside the binary, keyed by pack name. Pack tools load
/// through the same validation and registry as user tools; a user tool with
/// the same name shadows the pack version.
const PACKS: &[(&str, &[&str])] = &[(
    "github",
    &[
        include_str!("packs/github/gh_pr_meta.yaml"),
        include_str!("packs/github/gh_pr_comment.yaml"),
        include_str!("packs/github/git_diff.yaml"),
        include_str!("packs/github/git_changed_files.yaml"),
    ],
)];

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

/// Pack tools plus user tools from `dirs`; a user tool shadows a pack tool
/// with the same name (pin or tweak a pack tool by copying it locally).
pub fn load_tools_with_packs(
    packs: &[String],
    dirs: &[PathBuf],
) -> Result<Vec<UserToolDoc>, String> {
    let mut docs = load_pack_tools(packs)?;
    for user_doc in load_user_tools(dirs)? {
        docs.retain(|d| d.name != user_doc.name);
        docs.push(user_doc);
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
        ToolKind::Cypher { query } => {
            if query.is_empty() {
                return Err("cypher tool needs a query".to_string());
            }
        }
    }
    Ok(())
}

// ── Registry ─────────────────────────────────────────────────────────────

pub struct UserToolRegistry {
    tools: Vec<UserToolDoc>,
    router: Arc<ModelRouter>,
    cypher: Option<Arc<dyn CypherExecutor>>,
}

impl UserToolRegistry {
    pub fn new(
        tools: Vec<UserToolDoc>,
        router: Arc<ModelRouter>,
        cypher: Option<Arc<dyn CypherExecutor>>,
    ) -> Self {
        Self {
            tools,
            router,
            cypher,
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
        let input_value = input.clone();
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
                role,
            } => {
                self.run_prompt(doc, prompt, system.as_deref(), *role, &roots)
                    .await
            }
            ToolKind::Cypher { query } => self.run_cypher(query, &input_value).await,
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

    async fn run_prompt(
        &self,
        doc: &UserToolDoc,
        prompt: &str,
        system: Option<&str>,
        role: PromptRole,
        roots: &Roots<'_>,
    ) -> Result<ToolOutcome, ToolError> {
        let rendered =
            render_str(prompt, roots).map_err(|e| ToolError::Transport(e.to_string()))?;
        let request = ChatRequest {
            system: system.unwrap_or_default().to_string(),
            messages: vec![ChatMessage::User { content: rendered }],
            response_schema: doc.output_schema.as_ref().map(|schema| ResponseSchema {
                name: doc.name.clone(),
                schema: schema.clone(),
            }),
            ..Default::default()
        };
        let response = self
            .router
            .chat(role.into(), request)
            .await
            .map_err(|e| ToolError::Transport(e.to_string()))?;
        let result = match response.structured {
            Some(structured) => structured,
            None => json!({"text": response.content.unwrap_or_default()}),
        };
        Ok(ToolOutcome {
            result,
            is_error: false,
        })
    }

    async fn run_cypher(&self, query: &str, input: &Value) -> Result<ToolOutcome, ToolError> {
        let Some(cypher) = &self.cypher else {
            return Ok(ToolOutcome {
                result: json!({"error": "cypher tools require the ladybug storage backend"}),
                is_error: true,
            });
        };
        // Bind every input field as a $param; extra params are harmless.
        let params: Vec<(String, Value)> = input
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let rows = cypher.query(query, params).await?;
        Ok(ToolOutcome {
            result: json!({"rows": rows, "count": rows.len()}),
            is_error: false,
        })
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
            .map(|doc| ToolDef {
                name: format!("{USER_TOOL_PREFIX}{}", doc.name),
                description: doc.description.clone(),
                input_schema: doc
                    .input_schema
                    .clone()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                output_schema: doc.output_schema.clone(),
                output_example: None,
                read_only: doc.read_only.or(match &doc.kind {
                    ToolKind::Cypher { .. } | ToolKind::Prompt { .. } => Some(true),
                    ToolKind::Exec { .. } => None,
                }),
            })
            .collect())
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let Some(bare) = name.strip_prefix(USER_TOOL_PREFIX) else {
            return Err(ToolError::Unknown(name.to_string()));
        };
        let Some(doc) = self.tools.iter().find(|d| d.name == bare) else {
            return Err(ToolError::Unknown(name.to_string()));
        };
        self.run(doc, input).await
    }
}
