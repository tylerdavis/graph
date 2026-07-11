//! Layered TOML configuration for the graph CLI.
//!
//! Precedence (later wins): ~/.config/graph/config.toml < ./.graph/config.toml
//! < GRAPH_* environment variables < CLI flags.

mod load;
mod model;

pub use load::{
    expand_tilde, global_config_path, load, load_from, project_config_path, LoadedConfig,
};
pub use model::{
    Config, McpServerConfig, ModelChoice, ModelRoles, PlanPaths, ProviderConfig, ProviderKind,
    Role, Settings, StorageBackend, StorageConfig, ToolOverride, ToolPaths, UserConfig,
};
