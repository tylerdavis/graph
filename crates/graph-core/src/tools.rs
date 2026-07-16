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

/// Wraps a registry and hides a fixed set of tool names from `tools()`,
/// and refuses to invoke them (returns `ToolError::Unknown`). Used to
/// scope a tool out of one surface (e.g. the workbench) without removing
/// it from the shared catalog.
pub struct ExcludingRegistry {
    inner: std::sync::Arc<dyn ToolRegistry>,
    hidden: Vec<String>,
}

impl ExcludingRegistry {
    pub fn new(inner: std::sync::Arc<dyn ToolRegistry>, hidden: Vec<String>) -> Self {
        Self { inner, hidden }
    }
}

#[async_trait]
impl ToolRegistry for ExcludingRegistry {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut defs = self.inner.tools().await?;
        defs.retain(|d| !self.hidden.iter().any(|h| h == &d.name));
        Ok(defs)
    }
    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        if self.hidden.iter().any(|h| h == name) {
            return Err(ToolError::Unknown(name.to_string()));
        }
        self.inner.invoke(name, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }
    }

    struct MockRegistry {
        defs: Vec<ToolDef>,
    }

    #[async_trait]
    impl ToolRegistry for MockRegistry {
        async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
            Ok(self.defs.clone())
        }

        async fn invoke(&self, name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
            if self.defs.iter().any(|d| d.name == name) {
                Ok(ToolOutcome {
                    result: Value::String(name.to_string()),
                    is_error: false,
                })
            } else {
                Err(ToolError::Unknown(name.to_string()))
            }
        }
    }

    #[tokio::test]
    async fn excluding_registry_hides_and_refuses() {
        let inner: std::sync::Arc<dyn ToolRegistry> = std::sync::Arc::new(MockRegistry {
            defs: vec![def("a"), def("plan_and_execute")],
        });
        let reg = ExcludingRegistry::new(inner, vec!["plan_and_execute".to_string()]);

        let names: Vec<String> = reg
            .tools()
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["a".to_string()]);

        let err = reg
            .invoke("plan_and_execute", Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Unknown(name) if name == "plan_and_execute"));

        let outcome = reg.invoke("a", Value::Null).await.unwrap();
        assert_eq!(outcome.result, Value::String("a".to_string()));
    }
}
