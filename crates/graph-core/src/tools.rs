//! The tool abstraction shared by the agent loop and the plan pipeline.
//!
//! Implementations: MCP servers (graph-mcp), built-in graph tools
//! (graph-store), user-defined tools, and plan tools.

use async_trait::async_trait;
use serde_json::Value;

/// A tool as seen by the model: namespaced name, description, and schemas.
#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Namespaced: `github__search_issues`, `user__git_log`, …
    pub name: String,
    pub description: String,
    /// JSON Schema for the input.
    pub input_schema: Value,
    /// JSON Schema for the output, when the source declares one.
    pub output_schema: Option<Value>,
    /// Example output, when available (feeds plan validation).
    pub output_example: Option<Value>,
    /// True when the source annotates the tool as read-only.
    pub read_only: Option<bool>,
}

/// The result of invoking a tool. `is_error` results flow back to the model
/// (agent loop) or onto the bus (pipeline) rather than failing the run.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub result: Value,
    pub is_error: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("tool transport failed: {0}")]
    Transport(String),
}

#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// Every tool this registry exposes, already namespaced and filtered.
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError>;

    /// Invoke a tool by its namespaced name.
    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError>;
}

/// Merge several registries into one catalog. Invocation tries each in
/// order; `Unknown` falls through to the next.
pub struct CompositeRegistry {
    registries: Vec<std::sync::Arc<dyn ToolRegistry>>,
}

impl CompositeRegistry {
    pub fn new(registries: Vec<std::sync::Arc<dyn ToolRegistry>>) -> Self {
        Self { registries }
    }
}

#[async_trait]
impl ToolRegistry for CompositeRegistry {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut all = Vec::new();
        for registry in &self.registries {
            all.extend(registry.tools().await?);
        }
        Ok(all)
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        for registry in &self.registries {
            match registry.invoke(name, input.clone()).await {
                Err(ToolError::Unknown(_)) => continue,
                other => return other,
            }
        }
        Err(ToolError::Unknown(name.to_string()))
    }
}
