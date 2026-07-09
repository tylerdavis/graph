//! ToolRegistry decorator that feeds the observed-shape cache: every
//! successful tool call records an inferred output schema + truncated
//! example, so the planner (phase 4) learns tool shapes from everyday use.

use graph_core::shapes::{infer_schema, truncate_example};
use graph_core::store::Store;
use graph_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::Value;
use std::sync::Arc;

pub struct RecordingRegistry {
    inner: Arc<dyn ToolRegistry>,
    store: Arc<dyn Store>,
}

impl RecordingRegistry {
    pub fn new(inner: Arc<dyn ToolRegistry>, store: Arc<dyn Store>) -> Self {
        Self { inner, store }
    }
}

#[async_trait::async_trait]
impl ToolRegistry for RecordingRegistry {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        self.inner.tools().await
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let outcome = self.inner.invoke(name, input).await?;
        if !outcome.is_error {
            let schema = infer_schema(&outcome.result);
            let example = truncate_example(&outcome.result);
            if let Err(e) = self.store.record_tool_shape(name, &schema, &example).await {
                tracing::debug!(tool = name, error = %e, "shape recording failed");
            }
        }
        Ok(outcome)
    }
}
