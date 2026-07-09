//! Provider-neutral chat types.

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One message in a conversation, shaped to map onto both the Anthropic
/// content-block model and the OpenAI tool-call model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatMessage {
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// The result of executing one tool call, fed back to the model.
    ToolResult {
        tool_call_id: String,
        /// JSON result (objects preferred; plain text wrapped as a string).
        content: Value,
        #[serde(default)]
        is_error: bool,
    },
}

/// A tool made visible to the model via its native tool-use API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: Value,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Structured-output request: forces the model to produce JSON matching
/// `schema`. Providers enforce this natively where possible.
#[derive(Debug, Clone)]
pub struct ResponseSchema {
    pub name: String,
    pub schema: Value,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// When set, the provider forces schema-conforming JSON output and the
    /// response carries it in `ChatResponse::structured`.
    pub response_schema: Option<ResponseSchema>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    #[default]
    Other,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Present when the request carried a `response_schema`.
    pub structured: Option<Value>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Usage,
}

/// Streaming events. Providers assemble tool-call JSON internally and deliver
/// complete calls in the final `Completed` response; only text streams.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    /// The model started emitting a tool call (name known, arguments pending).
    ToolCallStarted {
        name: String,
    },
    Completed(ChatResponse),
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, crate::LlmError>>;
