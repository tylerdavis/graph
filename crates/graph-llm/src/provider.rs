use crate::types::{ChatRequest, ChatResponse, EventStream};
use crate::LlmError;
use async_trait::async_trait;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError>;

    /// Stream text deltas and tool-call start notices, ending with a
    /// `StreamEvent::Completed` carrying the assembled response.
    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError>;
}
