//! OpenAI-compatible Chat Completions provider (Ollama, vLLM, LM Studio,
//! and OpenAI itself via the default base URL).
//!
//! Structured output tries `response_format: json_schema` first; servers
//! that reject it (HTTP 4xx) are retried with `json_object` plus the schema
//! embedded in the system prompt, and the working mode is remembered.

use crate::types::*;
use crate::{ChatProvider, LlmError};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU8, Ordering};

const MODE_UNKNOWN: u8 = 0;
const MODE_JSON_SCHEMA: u8 = 1;
const MODE_JSON_OBJECT: u8 = 2;

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    structured_mode: AtomicU8,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            structured_mode: AtomicU8::new(MODE_UNKNOWN),
        }
    }

    fn build_body(&self, req: &ChatRequest, stream: bool, mode: u8) -> Value {
        let mut system = req.system.clone();
        let mut body = Map::new();
        body.insert("model".into(), json!(req.model));
        if let Some(t) = req.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(m) = req.max_tokens {
            body.insert("max_tokens".into(), json!(m));
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body.insert("tools".into(), json!(tools));
        }
        if let Some(schema) = &req.response_schema {
            match mode {
                MODE_JSON_OBJECT => {
                    body.insert("response_format".into(), json!({"type": "json_object"}));
                    system.push_str(&format!(
                        "\n\nRespond with a single JSON object conforming to this JSON Schema, and nothing else:\n{}",
                        schema.schema
                    ));
                }
                _ => {
                    body.insert(
                        "response_format".into(),
                        json!({
                            "type": "json_schema",
                            "json_schema": {
                                "name": schema.name,
                                "schema": schema.schema,
                                "strict": true,
                            }
                        }),
                    );
                }
            }
        }
        let mut messages = vec![json!({"role": "system", "content": system})];
        messages.extend(to_openai_messages(&req.messages));
        body.insert("messages".into(), json!(messages));
        if stream {
            body.insert("stream".into(), json!(true));
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        Value::Object(body)
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
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

    /// POST with structured-output mode fallback: on a 4xx while in
    /// json_schema mode, downgrade to json_object and retry once.
    async fn post_with_fallback(
        &self,
        req: &ChatRequest,
        stream: bool,
    ) -> Result<reqwest::Response, LlmError> {
        let mode = self.structured_mode.load(Ordering::Relaxed);
        let first_mode = if mode == MODE_UNKNOWN {
            MODE_JSON_SCHEMA
        } else {
            mode
        };
        let body = self.build_body(req, stream, first_mode);
        match self.post(&body).await {
            Ok(response) => {
                self.structured_mode.store(first_mode, Ordering::Relaxed);
                Ok(response)
            }
            Err(LlmError::Api { status, .. })
                if req.response_schema.is_some()
                    && first_mode == MODE_JSON_SCHEMA
                    && (400..500).contains(&status) =>
            {
                let body = self.build_body(req, stream, MODE_JSON_OBJECT);
                let response = self.post(&body).await?;
                self.structured_mode
                    .store(MODE_JSON_OBJECT, Ordering::Relaxed);
                Ok(response)
            }
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAiCompatProvider {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let value: Value = self.post_with_fallback(&req, false).await?.json().await?;
        parse_response(&value, req.response_schema.is_some())
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let wants_structured = req.response_schema.is_some();
        let response = self.post_with_fallback(&req, true).await?;
        let mut assembler = StreamAssembler {
            wants_structured,
            ..Default::default()
        };
        let stream = response
            .bytes_stream()
            .eventsource()
            .map(move |event| match event {
                Err(e) => vec![Err(LlmError::Parse(e.to_string()))],
                Ok(event) => assembler.handle(&event.data),
            })
            .flat_map(futures::stream::iter);
        Ok(stream.boxed())
    }
}

#[derive(Default)]
struct StreamAssembler {
    text: String,
    /// Tool calls accumulated by stream index: (id, name, arguments JSON).
    tool_calls: Vec<(String, String, String)>,
    finish_reason: Option<String>,
    usage: Usage,
    wants_structured: bool,
    done: bool,
}

impl StreamAssembler {
    fn handle(&mut self, data: &str) -> Vec<Result<StreamEvent, LlmError>> {
        if data.trim() == "[DONE]" {
            return if self.done {
                vec![]
            } else {
                vec![self.finish()]
            };
        }
        let parsed: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => return vec![Err(LlmError::Parse(format!("bad SSE payload: {e}")))],
        };
        if let Some(usage) = parsed.get("usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage.output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = parsed["choices"].get(0) else {
            return vec![];
        };
        let mut events = Vec::new();
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                self.text.push_str(text);
                events.push(Ok(StreamEvent::TextDelta(text.to_string())));
            }
        }
        for call in delta["tool_calls"].as_array().cloned().unwrap_or_default() {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            while self.tool_calls.len() <= index {
                self.tool_calls
                    .push((String::new(), String::new(), String::new()));
            }
            let slot = &mut self.tool_calls[index];
            if let Some(id) = call["id"].as_str() {
                slot.0 = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str() {
                slot.1.push_str(name);
                events.push(Ok(StreamEvent::ToolCallStarted {
                    name: slot.1.clone(),
                }));
            }
            if let Some(fragment) = call["function"]["arguments"].as_str() {
                slot.2.push_str(fragment);
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        events
    }

    fn finish(&mut self) -> Result<StreamEvent, LlmError> {
        self.done = true;
        let mut tool_calls = Vec::new();
        for (id, name, raw) in self.tool_calls.drain(..) {
            let arguments: Value = if raw.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&raw)
                    .map_err(|e| LlmError::Parse(format!("tool arguments for {name}: {e}")))?
            };
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        let text = std::mem::take(&mut self.text);
        let structured = if self.wants_structured && !text.is_empty() {
            Some(parse_json_content(&text)?)
        } else {
            None
        };
        Ok(StreamEvent::Completed(ChatResponse {
            content: (!text.is_empty() && structured.is_none()).then_some(text),
            tool_calls,
            structured,
            stop_reason: match self.finish_reason.as_deref() {
                Some("tool_calls") => StopReason::ToolUse,
                Some("length") => StopReason::MaxTokens,
                Some("stop") => StopReason::EndTurn,
                _ => StopReason::Other,
            },
            usage: std::mem::take(&mut self.usage),
        }))
    }
}

fn to_openai_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| match message {
            ChatMessage::User { content } => json!({"role": "user", "content": content}),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert("content".into(), json!(content));
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    "arguments": c.arguments.to_string(),
                                }
                            })
                        })
                        .collect();
                    msg.insert("tool_calls".into(), json!(calls));
                }
                Value::Object(msg)
            }
            ChatMessage::ToolResult {
                tool_call_id,
                content,
                is_error: _,
            } => {
                let rendered = match content {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                json!({"role": "tool", "tool_call_id": tool_call_id, "content": rendered})
            }
        })
        .collect()
}

fn parse_response(value: &Value, wants_structured: bool) -> Result<ChatResponse, LlmError> {
    let choice = value["choices"]
        .get(0)
        .ok_or_else(|| LlmError::Parse("response has no choices".into()))?;
    let message = &choice["message"];
    let text = message["content"].as_str().unwrap_or_default().to_string();

    let mut tool_calls = Vec::new();
    for call in message["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let name = call["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
        let arguments = serde_json::from_str(raw)
            .map_err(|e| LlmError::Parse(format!("tool arguments for {name}: {e}")))?;
        tool_calls.push(ToolCall {
            id: call["id"].as_str().unwrap_or_default().to_string(),
            name,
            arguments,
        });
    }

    let structured = if wants_structured && !text.is_empty() {
        Some(parse_json_content(&text)?)
    } else {
        None
    };

    Ok(ChatResponse {
        content: (!text.is_empty() && structured.is_none()).then_some(text),
        tool_calls,
        structured,
        stop_reason: match choice["finish_reason"].as_str() {
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("stop") => StopReason::EndTurn,
            _ => StopReason::Other,
        },
        usage: Usage {
            input_tokens: value["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: value["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
    })
}

/// Parse model text as JSON, tolerating markdown code fences that weaker
/// local models wrap JSON in despite json mode.
fn parse_json_content(text: &str) -> Result<Value, LlmError> {
    let trimmed = text.trim();
    let candidate = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    serde_json::from_str(candidate).map_err(|e| LlmError::SchemaMismatch(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_json_schema_then_falls_back_to_json_object_body() {
        let provider = OpenAiCompatProvider::new("http://localhost:11434/v1".into(), None);
        let req = ChatRequest {
            model: "llama3".into(),
            system: "You are a planner.".into(),
            response_schema: Some(ResponseSchema {
                name: "plan".into(),
                schema: json!({"type": "object"}),
            }),
            ..Default::default()
        };
        let strict = provider.build_body(&req, false, MODE_JSON_SCHEMA);
        assert_eq!(strict["response_format"]["type"], "json_schema");

        let fallback = provider.build_body(&req, false, MODE_JSON_OBJECT);
        assert_eq!(fallback["response_format"]["type"], "json_object");
        let system = fallback["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("JSON Schema"));
    }

    #[test]
    fn stream_assembler_reassembles_split_tool_calls() {
        let mut assembler = StreamAssembler::default();
        assembler.handle(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"search","arguments":""}}]}}]}"#);
        assembler.handle(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\"ru"}}]}}]}"#);
        assembler.handle(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"st\"}"}}]}}]}"#);
        assembler.handle(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
        let events = assembler.handle("[DONE]");
        let StreamEvent::Completed(response) = events[0].as_ref().unwrap() else {
            panic!("expected Completed");
        };
        assert_eq!(response.tool_calls[0].arguments, json!({"q": "rust"}));
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn parses_fenced_json_from_weak_models() {
        let parsed = parse_json_content("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(parsed, json!({"a": 1}));
    }

    #[test]
    fn parses_non_streaming_tool_call_response() {
        let value = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "function": {"name": "lookup", "arguments": "{\"id\": 4}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3}
        });
        let parsed = parse_response(&value, false).unwrap();
        assert_eq!(parsed.tool_calls[0].arguments, json!({"id": 4}));
        assert_eq!(parsed.usage.input_tokens, 7);
    }
}
