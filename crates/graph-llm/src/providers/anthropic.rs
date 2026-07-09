//! Anthropic Messages API provider.
//!
//! Structured output is enforced by forcing a single synthetic tool
//! (`tool_choice: {type: "tool"}`) whose input schema is the response schema.

use crate::types::*;
use crate::{ChatProvider, LlmError};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Map, Value};

const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;
const STRUCTURED_TOOL: &str = "structured_output";

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            api_key,
        }
    }

    fn build_body(&self, req: &ChatRequest, stream: bool) -> Value {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let mut body = Map::new();
        body.insert("model".into(), json!(req.model));
        body.insert(
            "max_tokens".into(),
            json!(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        );
        body.insert(
            "messages".into(),
            json!(to_anthropic_messages(&req.messages)),
        );
        if !req.system.is_empty() {
            body.insert("system".into(), json!(req.system));
        }
        if let Some(t) = req.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(schema) = &req.response_schema {
            tools.push(json!({
                "name": STRUCTURED_TOOL,
                "description": format!("Produce the final {} object.", schema.name),
                "input_schema": schema.schema,
            }));
            body.insert(
                "tool_choice".into(),
                json!({"type": "tool", "name": STRUCTURED_TOOL}),
            );
        }
        if !tools.is_empty() {
            body.insert("tools".into(), json!(tools));
        }
        if stream {
            body.insert("stream".into(), json!(true));
        }
        Value::Object(body)
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }

    /// POST, retrying once without `temperature` when the model rejects it
    /// (newer models deprecate the parameter; per-role config may still set
    /// it for models that accept it).
    async fn post_with_retry(&self, mut body: Value) -> Result<reqwest::Response, LlmError> {
        match self.post(&body).await {
            Err(LlmError::Api {
                status: 400,
                body: message,
            }) if message.contains("temperature") && body.get("temperature").is_some() => {
                body.as_object_mut().unwrap().remove("temperature");
                tracing::debug!("model rejected temperature; retrying without it");
                self.post(&body).await
            }
            other => other,
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let body = self.build_body(&req, false);
        let value: Value = self.post_with_retry(body).await?.json().await?;
        parse_response(&value)
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let body = self.build_body(&req, true);
        let response = self.post_with_retry(body).await?;
        let mut assembler = StreamAssembler::default();

        let stream = response
            .bytes_stream()
            .eventsource()
            .map(move |event| match event {
                Err(e) => vec![Err(LlmError::Parse(e.to_string()))],
                Ok(event) => assembler.handle(&event.event, &event.data),
            })
            .flat_map(futures::stream::iter);
        Ok(stream.boxed())
    }
}

/// Assembles Anthropic SSE events into `StreamEvent`s, accumulating
/// tool-call input JSON until the message completes.
#[derive(Default)]
struct StreamAssembler {
    text: String,
    /// (id, name, partial JSON) per content-block index.
    tool_blocks: Vec<(String, String, String)>,
    /// Maps content-block index → position in `tool_blocks` (text blocks are None).
    block_kinds: Vec<Option<usize>>,
    stop_reason: StopReason,
    usage: Usage,
}

impl StreamAssembler {
    fn handle(&mut self, event: &str, data: &str) -> Vec<Result<StreamEvent, LlmError>> {
        let parsed: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) if data.is_empty() => return vec![],
            Err(e) => return vec![Err(LlmError::Parse(format!("bad SSE payload: {e}")))],
        };
        match event {
            "message_start" => {
                if let Some(input) = parsed["message"]["usage"]["input_tokens"].as_u64() {
                    self.usage.input_tokens = input;
                }
                vec![]
            }
            "content_block_start" => {
                let block = &parsed["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap_or_default().to_string();
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        self.tool_blocks.push((id, name.clone(), String::new()));
                        self.block_kinds.push(Some(self.tool_blocks.len() - 1));
                        vec![Ok(StreamEvent::ToolCallStarted { name })]
                    }
                    _ => {
                        self.block_kinds.push(None);
                        vec![]
                    }
                }
            }
            "content_block_delta" => {
                let delta = &parsed["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default().to_string();
                        self.text.push_str(&text);
                        vec![Ok(StreamEvent::TextDelta(text))]
                    }
                    Some("input_json_delta") => {
                        let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(Some(slot)) = self.block_kinds.get(index) {
                            if let Some(block) = self.tool_blocks.get_mut(*slot) {
                                block
                                    .2
                                    .push_str(delta["partial_json"].as_str().unwrap_or(""));
                            }
                        }
                        vec![]
                    }
                    _ => vec![],
                }
            }
            "message_delta" => {
                if let Some(reason) = parsed["delta"]["stop_reason"].as_str() {
                    self.stop_reason = map_stop_reason(reason);
                }
                if let Some(output) = parsed["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = output;
                }
                vec![]
            }
            "message_stop" => vec![self.finish()],
            "error" => vec![Err(LlmError::Api {
                status: 0,
                body: data.to_string(),
            })],
            _ => vec![],
        }
    }

    fn finish(&mut self) -> Result<StreamEvent, LlmError> {
        let mut tool_calls = Vec::new();
        let mut structured = None;
        for (id, name, raw) in self.tool_blocks.drain(..) {
            let arguments: Value = if raw.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&raw)
                    .map_err(|e| LlmError::Parse(format!("tool input for {name}: {e}")))?
            };
            if name == STRUCTURED_TOOL {
                structured = Some(arguments);
            } else {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
        }
        Ok(StreamEvent::Completed(ChatResponse {
            content: (!self.text.is_empty()).then(|| std::mem::take(&mut self.text)),
            tool_calls,
            structured,
            stop_reason: self.stop_reason,
            usage: std::mem::take(&mut self.usage),
        }))
    }
}

fn to_anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        match message {
            ChatMessage::User { content } => {
                out.push(json!({"role": "user", "content": content}));
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                if let Some(text) = content {
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                for call in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            ChatMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let rendered = match content {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": rendered,
                    "is_error": is_error,
                });
                // Consecutive tool results must share one user message.
                match out.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["content"].as_array().is_some_and(|blocks| {
                                blocks.iter().all(|b| b["type"] == "tool_result")
                            }) =>
                    {
                        last["content"].as_array_mut().unwrap().push(block);
                    }
                    _ => out.push(json!({"role": "user", "content": [block]})),
                }
            }
        }
    }
    out
}

fn parse_response(value: &Value) -> Result<ChatResponse, LlmError> {
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut structured = None;

    for block in value["content"].as_array().cloned().unwrap_or_default() {
        match block["type"].as_str() {
            Some("text") => content.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => {
                let name = block["name"].as_str().unwrap_or_default().to_string();
                if name == STRUCTURED_TOOL {
                    structured = Some(block["input"].clone());
                } else {
                    tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name,
                        arguments: block["input"].clone(),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(ChatResponse {
        content: (!content.is_empty()).then_some(content),
        tool_calls,
        structured,
        stop_reason: value["stop_reason"]
            .as_str()
            .map(map_stop_reason)
            .unwrap_or(StopReason::Other),
        usage: Usage {
            input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0),
        },
    })
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new("test-key".into(), None)
    }

    #[test]
    fn structured_output_forces_synthetic_tool() {
        let req = ChatRequest {
            model: "claude-sonnet-5".into(),
            response_schema: Some(ResponseSchema {
                name: "plan".into(),
                schema: json!({"type": "object"}),
            }),
            ..Default::default()
        };
        let body = provider().build_body(&req, false);
        assert_eq!(body["tool_choice"]["name"], STRUCTURED_TOOL);
        assert_eq!(body["tools"][0]["name"], STRUCTURED_TOOL);
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let messages = vec![
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![
                    ToolCall {
                        id: "a".into(),
                        name: "t1".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "t2".into(),
                        arguments: json!({}),
                    },
                ],
            },
            ChatMessage::ToolResult {
                tool_call_id: "a".into(),
                content: json!({"x": 1}),
                is_error: false,
            },
            ChatMessage::ToolResult {
                tool_call_id: "b".into(),
                content: json!("plain"),
                is_error: false,
            },
        ];
        let rendered = to_anthropic_messages(&messages);
        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn parses_tool_use_response() {
        let value = json!({
            "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "tc1", "name": "github__list_prs", "input": {"state": "open"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20},
        });
        let parsed = parse_response(&value).unwrap();
        assert_eq!(parsed.content.as_deref(), Some("Let me check."));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "github__list_prs");
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn stream_assembler_accumulates_tool_input_json() {
        let mut assembler = StreamAssembler::default();
        assembler.handle(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":5}}}"#,
        );
        assembler.handle(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"tc1","name":"search"}}"#,
        );
        assembler.handle(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
        );
        assembler.handle(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}"#,
        );
        assembler.handle(
            "message_delta",
            r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
        );
        let events = assembler.handle("message_stop", "{}");
        let StreamEvent::Completed(response) = events[0].as_ref().unwrap() else {
            panic!("expected Completed");
        };
        assert_eq!(response.tool_calls[0].arguments, json!({"q": "rust"}));
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage.output_tokens, 9);
    }
}
