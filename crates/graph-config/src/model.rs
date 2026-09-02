//! Serde model for config.toml.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    /// Named provider connections, e.g. `[providers.anthropic]`.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Per-role model assignment, e.g. `planner = { provider = "...", model = "..." }`.
    #[serde(default)]
    pub models: ModelRoles,
    /// MCP server definitions, e.g. `[mcp.github]`.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub plans: PlanPaths,
    #[serde(default)]
    pub tools: ToolPaths,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub user: UserConfig,
    #[serde(default)]
    pub prompts: PromptConfig,
    #[serde(default)]
    pub workbench: WorkbenchConfig,
    /// Per-model token prices, e.g. `[pricing."claude-sonnet-5"]`. Keyed by
    /// the model id as written in `[models]` — that is what goes on the wire.
    /// Absent prices mean usage is reported in tokens with no dollar figure.
    #[serde(default)]
    pub pricing: BTreeMap<String, ModelPrice>,
}

/// What one model costs, in USD per million tokens.
///
/// Deliberately not shipped with built-in defaults: published prices change,
/// and a stale table that quietly reports the wrong dollar figure is worse
/// than reporting none. An unpriced model still reports its token counts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// Uncached input tokens.
    pub input: f64,
    pub output: f64,
    /// Tokens written to the prompt cache. Defaults to `input` x 1.25, the
    /// standard premium for a 5-minute TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Tokens served from the prompt cache. Defaults to `input` x 0.10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
}

impl ModelPrice {
    /// Cache-write rate, falling back to the standard 1.25x premium.
    pub fn cache_write_rate(&self) -> f64 {
        self.cache_write.unwrap_or(self.input * 1.25)
    }

    /// Cache-read rate, falling back to the standard 0.1x rate.
    ///
    /// Divides rather than multiplying by 0.1: `3.0 * 0.1` lands a rounding
    /// step away from `0.3`, and a rate that prints as `0.30000000000000004`
    /// invites doubt about the whole cost figure.
    pub fn cache_read_rate(&self) -> f64 {
        self.cache_read.unwrap_or(self.input / 10.0)
    }
}

/// System-prompt overrides. Each field replaces the built-in text
/// wholesale; leave unset to keep the default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PromptConfig {
    /// Base system prompt for the chat/ask agent loop. The current
    /// date/time and `[user]` name/context are still appended after it.
    pub chat: Option<String>,
    /// Workbench framing and policy, appended to the chat prompt inside
    /// `graph workbench`. The `workbench__*` tool rules are appended
    /// after it and are not overridable.
    pub workbench: Option<String>,
}

/// `graph workbench` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkbenchConfig {
    /// Where the workbench writes its debug log (tilde-expanded). Default:
    /// `<data_dir>/workbench.log`; the `GRAPH_WORKBENCH_LOG` env var wins
    /// over both.
    pub log_path: Option<PathBuf>,
}

/// Runtime-state storage. Defaults to plain files under `data_dir`, so a
/// fresh install needs zero configuration; `memory` runs ephemeral (CI
/// jobs, tests). Centralized backends (postgres/remote) slot in behind the
/// same trait.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    pub backend: StorageBackend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageBackend {
    #[default]
    File,
    Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub max_agent_iterations: u32,
    pub planning_attempts: u32,
    pub history_limit: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("~/.local/share/graph"),
            max_agent_iterations: 15,
            planning_attempts: 2,
            history_limit: 20,
        }
    }
}

/// A `${VAR}` reference whose variable was unset when the config loaded.
///
/// Recorded instead of failing the load, for the sections whose values are
/// only needed when the entry is actually *used* (`[providers.*]`,
/// `[mcp.*]`). The value keeps its literal `${VAR}` text; the component that
/// owns the entry reports this — with the variable name — the moment the
/// entry is exercised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingEnv {
    /// The field inside the entry, e.g. `api_key` or `headers.Authorization`.
    pub field: String,
    /// The environment variable that was not set.
    pub var: String,
}

impl MissingEnv {
    /// One sentence naming the variable and where the config references it,
    /// e.g. `environment variable ANTHROPIC_API_KEY (providers.anthropic.api_key) is not set`.
    pub fn describe(&self, entry: &str) -> String {
        format!(
            "environment variable {} ({entry}.{}) is not set",
            self.var, self.field
        )
    }
}

/// Render every [`MissingEnv`] of one entry as a single reason string.
pub fn describe_missing_env(entry: &str, missing: &[MissingEnv]) -> String {
    missing
        .iter()
        .map(|m| m.describe(entry))
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    /// API key; supports `${ENV_VAR}` expansion.
    pub api_key: Option<String>,
    /// Base URL for `openai_compat` (e.g. Ollama at http://localhost:11434/v1).
    pub base_url: Option<String>,
    /// Bedrock only.
    pub region: Option<String>,
    /// Bedrock only: AWS shared-config profile name.
    pub profile: Option<String>,
    /// Unset `${VAR}` references in this entry, recorded at load. A provider
    /// with any is unusable and says so when a model resolves to it.
    #[serde(skip)]
    pub missing_env: Vec<MissingEnv>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    Openai,
    OpenaiCompat,
    Bedrock,
}

/// A role's resolved model choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
    pub temperature: Option<f32>,
    /// Embedding dimension; only meaningful for the embedder role.
    pub dimensions: Option<u32>,
    /// What this model is good for. Surfaced to the planner as a routing
    /// signal wherever named models are selectable (e.g. `builtin__infer`'s
    /// `model` input), so write it for that audience.
    pub description: Option<String>,
    /// Failover candidates, tried in order when this model's provider is
    /// down (transient errors after its own retries are exhausted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<FallbackChoice>,
}

/// One failover candidate for a [`ModelChoice`]. Deliberately narrower than
/// `ModelChoice`: no description (never planner-routed on its own) and no
/// nested fallbacks (one flat chain per entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackChoice {
    pub provider: String,
    pub model: String,
    /// Overrides the request temperature when set; otherwise the primary's
    /// effective temperature carries over.
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelRoles {
    pub default: Option<ModelChoice>,
    pub chat: Option<ModelChoice>,
    pub planner: Option<ModelChoice>,
    pub solver: Option<ModelChoice>,
    pub use_case_solver: Option<ModelChoice>,
    pub repair: Option<ModelChoice>,
    pub embedder: Option<ModelChoice>,
    /// Cheap verdict calls for inferred exit gates.
    pub judge: Option<ModelChoice>,
    /// User-defined named models (`[models.named.<name>]`), referenceable
    /// wherever a model name is accepted (prompt-tool `model`,
    /// `builtin__infer`'s `model` input). Names must not shadow the role
    /// names above — enforced at config load.
    pub named: BTreeMap<String, ModelChoice>,
}

/// One pipeline/agent role that needs a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Chat,
    Planner,
    Solver,
    UseCaseSolver,
    Repair,
    Embedder,
    Judge,
}

impl Role {
    /// The role a config-facing name refers to, if any. These names are
    /// reserved: `[models.named]` entries may not shadow them.
    pub fn from_name(name: &str) -> Option<Role> {
        match name {
            "chat" => Some(Role::Chat),
            "planner" => Some(Role::Planner),
            "solver" => Some(Role::Solver),
            "use_case_solver" => Some(Role::UseCaseSolver),
            "repair" => Some(Role::Repair),
            "embedder" => Some(Role::Embedder),
            "judge" => Some(Role::Judge),
            _ => None,
        }
    }
}

/// Names `[models.named]` entries may not use: the role keys plus `default`.
pub const RESERVED_MODEL_NAMES: &[&str] = &[
    "default",
    "chat",
    "planner",
    "solver",
    "use_case_solver",
    "repair",
    "embedder",
    "judge",
];

impl ModelRoles {
    /// Resolve a role to its model choice, falling back to `default`.
    pub fn resolve(&self, role: Role) -> Option<&ModelChoice> {
        let specific = match role {
            Role::Chat => &self.chat,
            Role::Planner => &self.planner,
            Role::Solver => &self.solver,
            Role::UseCaseSolver => &self.use_case_solver,
            Role::Repair => &self.repair,
            Role::Embedder => &self.embedder,
            Role::Judge => &self.judge,
        };
        specific.as_ref().or(self.default.as_ref())
    }

    /// Every configured choice — the role slots plus `[models.named]`
    /// entries — for whole-config validation passes.
    pub fn all_choices(&self) -> impl Iterator<Item = &ModelChoice> {
        [
            &self.default,
            &self.chat,
            &self.planner,
            &self.solver,
            &self.use_case_solver,
            &self.repair,
            &self.embedder,
            &self.judge,
        ]
        .into_iter()
        .filter_map(Option::as_ref)
        .chain(self.named.values())
    }

    /// Resolve a model *name*: a role name (with its fallback to
    /// `default`), the literal `default`, or a `[models.named]` entry.
    pub fn resolve_name(&self, name: &str) -> Option<&ModelChoice> {
        if name == "default" {
            return self.default.as_ref();
        }
        match Role::from_name(name) {
            Some(role) => self.resolve(role),
            None => self.named.get(name),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// stdio transport: command to spawn.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the spawned process; values support `${ENV_VAR}`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Streamable-HTTP transport: server URL. Mutually exclusive with `command`.
    pub url: Option<String>,
    /// HTTP headers; values support `${ENV_VAR}`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Only expose these tools (exact names, pre-namespacing).
    pub include_tools: Option<Vec<String>>,
    /// Hide these tools.
    #[serde(default)]
    pub exclude_tools: Vec<String>,
    /// Output schema/example overrides keyed by tool name.
    #[serde(default)]
    pub tool_overrides: BTreeMap<String, ToolOverride>,
    /// Unset `${VAR}` references in this entry, recorded at load. A server
    /// with any refuses to connect and says so — a missing token must not
    /// silently become an empty header or child environment variable.
    #[serde(skip)]
    pub missing_env: Vec<MissingEnv>,
}

impl McpServerConfig {
    pub fn validate(&self, name: &str) -> Result<(), String> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => Err(format!(
                "mcp server '{name}': `command` and `url` are mutually exclusive"
            )),
            (None, None) => Err(format!(
                "mcp server '{name}': one of `command` (stdio) or `url` (http) is required"
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolOverride {
    pub description: Option<String>,
    pub output_schema: Option<serde_json::Value>,
    pub output_example: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PlanPaths {
    pub paths: Vec<PathBuf>,
}

impl Default for PlanPaths {
    fn default() -> Self {
        Self {
            paths: vec![
                PathBuf::from("./.graph/plans"),
                PathBuf::from("~/.config/graph/plans"),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolPaths {
    pub paths: Vec<PathBuf>,
    /// Bundled tool packs to enable (e.g. "github"). Pack tools ship inside
    /// the binary and load like user tools; a user tool with the same name
    /// shadows the pack version.
    pub packs: Vec<String>,
}

impl Default for ToolPaths {
    fn default() -> Self {
        Self {
            paths: vec![
                PathBuf::from("./.graph/tools"),
                PathBuf::from("~/.config/graph/tools"),
            ],
            packs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UserConfig {
    pub name: Option<String>,
    /// Freeform context injected into the chat and planner prompts.
    pub context: Option<String>,
    pub timezone: Option<String>,
}
