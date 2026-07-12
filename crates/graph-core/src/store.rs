//! Persistence abstraction: threads, messages, and the observed-shape cache.
//! Implemented by graph-store; behind a trait so backends (file, memory,
//! future remote stores) can be swapped.

use async_trait::async_trait;
use graph_llm::types::ChatMessage;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ThreadMeta {
    pub id: String,
    pub title: String,
    /// Epoch milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
}

/// An observed (or declared) output shape for a tool.
#[derive(Debug, Clone)]
pub struct ToolShape {
    pub tool: String,
    pub schema: Value,
    pub example: Value,
    pub seen_count: i64,
}

#[derive(Debug, thiserror::Error)]
#[error("store error: {0}")]
pub struct StoreError(pub String);

#[async_trait]
pub trait Store: Send + Sync {
    async fn create_thread(&self, title: &str) -> Result<ThreadMeta, StoreError>;
    async fn get_thread(&self, id: &str) -> Result<Option<ThreadMeta>, StoreError>;
    /// Most recently updated thread, if any.
    async fn latest_thread(&self) -> Result<Option<ThreadMeta>, StoreError>;
    /// Newest first.
    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, StoreError>;
    async fn delete_thread(&self, id: &str) -> Result<bool, StoreError>;

    /// Append messages to a thread and bump its updated_at.
    async fn append_messages(
        &self,
        thread_id: &str,
        messages: &[ChatMessage],
    ) -> Result<(), StoreError>;
    async fn load_messages(&self, thread_id: &str) -> Result<Vec<ChatMessage>, StoreError>;

    /// Record an observed output shape for a tool (upsert; bumps seen_count).
    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError>;
    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError>;
}
