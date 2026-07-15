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
}

/// System-prompt overrides. Each field replaces the built-in text
/// wholesale; leave unset to keep the default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PromptConfig {
    /// Base system prompt for the chat/ask agent loop. The current
    /// date/time and `[user]` name/context are still appended after it.
    pub chat: Option<String>,
    /// Workbench addendum appended to the chat prompt inside
    /// `graph workbench`. It must describe the `workbench__*` tools —
    /// they are how the agent operates on the draft.
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
                PathBuf::from("~/.config/graph/plans"),
                PathBuf::from("./.graph/plans"),
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
                PathBuf::from("~/.config/graph/tools"),
                PathBuf::from("./.graph/tools"),
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
