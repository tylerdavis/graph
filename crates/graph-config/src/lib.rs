//! Layered TOML configuration for the graph CLI.
//!
//! Precedence (later wins): ~/.config/graph/config.toml < ./.graph/config.toml
//! < GRAPH_* environment variables < CLI flags.

mod format;
mod load;
mod model;

pub use format::{
    inspect, migrate_file, migration_count, LayerInfo, Migrated, TooNew, Upgrade, CONFIG_FORMAT,
    CONFIG_FORMAT_OLDEST, FORMATS_DOC, FORMAT_KEY,
};
pub use load::{
    expand_tilde, global_config_path, load, load_from, project_config_path, LoadedConfig,
};
pub use model::{
    describe_missing_env, Config, FallbackChoice, McpServerConfig, MissingEnv, ModelChoice,
    ModelPrice, ModelRoles, PlanPaths, PromptConfig, ProviderConfig, ProviderKind, Role, Settings,
    StorageBackend, StorageConfig, ToolOverride, ToolPaths, UserConfig, WorkbenchConfig,
    RESERVED_MODEL_NAMES,
};
