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
/// Non-streaming ceiling: larger values risk HTTP timeouts on a single
/// response, per Anthropic's guidance (~16K without streaming).
const DEFAULT_MAX_TOKENS: u32 = 16_000;
/// Streaming default. max_tokens caps thinking + text COMBINED, and models
/// with adaptive thinking on by default (claude-sonnet-5 and newer) can
/// spend the whole budget thinking — an 8K cap yielded 90s turns that
/// ended with stop_reason max_tokens and zero visible text.
const DEFAULT_MAX_TOKENS_STREAMING: u32 = 64_000;
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

        let default_max_tokens = if stream {
            DEFAULT_MAX_TOKENS_STREAMING
        } else {
            DEFAULT_MAX_TOKENS
        };
        let caching = req.cache == CacheHint::Auto;
        let mut body = Map::new();
        body.insert("model".into(), json!(req.model));
        body.insert(
            "max_tokens".into(),
            json!(req.max_tokens.unwrap_or(default_max_tokens)),
        );

        let mut messages = to_anthropic_messages(&req.messages);
        if caching {
            // Breakpoint two: a rolling marker on the tail of the
            // conversation. On a multi-round loop this is the one that pays —
            // round N reads everything through round N-1 instead of
            // re-sending it, so input stops growing quadratically in rounds.
            mark_cache_breakpoint(&mut messages);
        }
        body.insert("messages".into(), json!(messages));

        if !req.system.is_empty() {
            // Breakpoint one. Render order is tools -> system -> messages, so
            // a marker on the system block covers the tool definitions too.
            body.insert(
                "system".into(),
                if caching {
                    json!([{
                        "type": "text",
                        "text": req.system,
                        "cache_control": ephemeral(),
                    }])
                } else {
                    json!(req.system)
                },
            );
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

    async fn post_once(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(crate::retry::api_error(response).await);
        }
        Ok(response)
    }

    /// POST with transient-failure retries (429/5xx/connect).
    async fn post(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        crate::retry::with_retries(|| self.post_once(body)).await
    }

    /// POST, retrying once without `temperature` when the model rejects it
    /// (newer models deprecate the parameter; per-role config may still set
    /// it for models that accept it).
    async fn post_with_retry(&self, mut body: Value) -> Result<reqwest::Response, LlmError> {
        match self.post(&body).await {
            Err(LlmError::Api {
                status: 400,
                body: message,
                ..
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
/// What a streamed content block is, and where its partial state lives.
#[derive(Clone, Copy)]
enum BlockKind {
    /// Text, or anything else with no reassembly state.
    Other,
    /// Position in `tool_blocks`.
    Tool(usize),
    /// Position in `thinking_blocks`.
    Thinking(usize),
}

#[derive(Default)]
struct StreamAssembler {
    text: String,
    /// (id, name, partial JSON) per content-block index.
    tool_blocks: Vec<(String, String, String)>,
    /// Reasoning blocks, reassembled in arrival order and replayed verbatim.
    /// With `display: "omitted"` — the default on current models — the text
    /// stays empty but the block and its signature still arrive, and still
    /// have to be echoed back.
    thinking_blocks: Vec<Value>,
    /// Maps content-block index → where that block's state lives.
    block_kinds: Vec<BlockKind>,
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
                // Carries the whole input side of the ledger, cache figures
                // included; `message_delta` revises `output_tokens` later.
                let usage = &parsed["message"]["usage"];
                if !usage.is_null() {
                    self.usage = parse_usage(usage);
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
                        self.block_kinds
                            .push(BlockKind::Tool(self.tool_blocks.len() - 1));
                        vec![Ok(StreamEvent::ToolCallStarted { name })]
                    }
                    // Complete on arrival — no deltas follow, so keep it as-is.
                    Some("redacted_thinking") => {
                        self.thinking_blocks.push(block.clone());
                        self.block_kinds
                            .push(BlockKind::Thinking(self.thinking_blocks.len() - 1));
                        vec![]
                    }
                    Some("thinking") => {
                        self.thinking_blocks.push(json!({
                            "type": "thinking",
                            "thinking": block["thinking"].as_str().unwrap_or_default(),
                            "signature": "",
                        }));
                        self.block_kinds
                            .push(BlockKind::Thinking(self.thinking_blocks.len() - 1));
                        vec![]
                    }
                    _ => {
                        self.block_kinds.push(BlockKind::Other);
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
                        if let Some(BlockKind::Tool(slot)) = self.block_kinds.get(index).copied() {
                            if let Some(block) = self.tool_blocks.get_mut(slot) {
                                block
                                    .2
                                    .push_str(delta["partial_json"].as_str().unwrap_or(""));
                            }
                        }
                        vec![]
                    }
                    // Reasoning arrives as text then a signature, on the same
                    // block index. Both have to be reassembled: a thinking
                    // block replayed without its signature is rejected.
                    Some(kind @ ("thinking_delta" | "signature_delta")) => {
                        let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(BlockKind::Thinking(slot)) =
                            self.block_kinds.get(index).copied()
                        {
                            if let Some(block) = self.thinking_blocks.get_mut(slot) {
                                if kind == "thinking_delta" {
                                    let fragment = delta["thinking"].as_str().unwrap_or_default();
                                    if let Some(text) =
                                        block["thinking"].as_str().map(str::to_string)
                                    {
                                        block["thinking"] = json!(text + fragment);
                                    }
                                } else {
                                    block["signature"] = delta["signature"].clone();
                                }
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
                retry_after: None,
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
            thinking: std::mem::take(&mut self.thinking_blocks),
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
                thinking,
            } => {
                // Thinking first: the API requires reasoning blocks to
                // precede text and tool_use in an assistant turn, and
                // replays them only if they are byte-identical to what it
                // sent.
                let mut blocks: Vec<Value> = thinking.clone();
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

/// Block types that carry model reasoning. Kept verbatim and replayed
/// unchanged — the API rejects modified blocks, and dropping them costs the
/// model its own reasoning between rounds of a tool-calling loop.
fn is_thinking_block(kind: Option<&str>) -> bool {
    matches!(kind, Some("thinking") | Some("redacted_thinking"))
}

fn parse_response(value: &Value) -> Result<ChatResponse, LlmError> {
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut structured = None;
    let mut thinking = Vec::new();

    for block in value["content"].as_array().cloned().unwrap_or_default() {
        match block["type"].as_str() {
            kind if is_thinking_block(kind) => thinking.push(block),
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
        thinking,
        structured,
        stop_reason: value["stop_reason"]
            .as_str()
            .map(map_stop_reason)
            .unwrap_or(StopReason::Other),
        usage: parse_usage(&value["usage"]),
    })
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

/// Put a cache breakpoint on the last content block of the final message.
///
/// A `User` message renders its content as a bare string, which has nowhere
/// to hang `cache_control`, so it is promoted to a single-element block array
/// first. An assistant turn with neither text nor tool calls has no block to
/// mark and is left alone — one fewer breakpoint, never a malformed request.
fn mark_cache_breakpoint(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    match &mut last["content"] {
        Value::String(text) => {
            let text = std::mem::take(text);
            last["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": ephemeral(),
            }]);
        }
        Value::Array(blocks) => {
            if let Some(block) = blocks.last_mut() {
                block["cache_control"] = ephemeral();
            }
        }
        _ => {}
    }
}

/// Reads an Anthropic `usage` object. Fields the response omits stay zero —
/// notably the two cache figures, which are absent entirely on a request that
/// carried no `cache_control` breakpoint.
fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
    }
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
    fn default_max_tokens_is_larger_when_streaming() {
        let req = ChatRequest {
            model: "claude-sonnet-5".into(),
            ..Default::default()
        };
        let body = provider().build_body(&req, true);
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS_STREAMING));
        let body = provider().build_body(&req, false);
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));

        // An explicit max_tokens always wins.
        let explicit = ChatRequest {
            model: "claude-sonnet-5".into(),
            max_tokens: Some(1024),
            ..Default::default()
        };
        assert_eq!(
            provider().build_body(&explicit, true)["max_tokens"],
            json!(1024)
        );
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

    /// Every `cache_control` marker anywhere in the request body.
    fn breakpoints(body: &Value) -> Vec<&Value> {
        fn walk<'a>(value: &'a Value, found: &mut Vec<&'a Value>) {
            match value {
                Value::Object(map) => {
                    if let Some(control) = map.get("cache_control") {
                        found.push(control);
                    }
                    map.values().for_each(|v| walk(v, found));
                }
                Value::Array(items) => items.iter().for_each(|v| walk(v, found)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        walk(body, &mut found);
        found
    }

    fn conversation() -> Vec<ChatMessage> {
        vec![
            ChatMessage::User {
                content: "the task, with a large stable brief".into(),
            },
            ChatMessage::Assistant {
                content: None,
                thinking: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "repo_grep".into(),
                    arguments: json!({"q": "Usage"}),
                }],
            },
            ChatMessage::ToolResult {
                tool_call_id: "t1".into(),
                content: json!("a match"),
                is_error: false,
            },
        ]
    }

    #[test]
    fn thinking_blocks_survive_a_round_trip_unchanged_and_come_first() {
        // The API replays reasoning only when the blocks are byte-identical
        // to what it sent, and rejects an assistant turn whose thinking does
        // not precede its text and tool_use.
        let response = parse_response(&json!({
            "content": [
                {"type": "thinking", "thinking": "let me check", "signature": "sig-abc"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "t1", "name": "search", "input": {"q": "x"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }))
        .unwrap();
        assert_eq!(response.thinking.len(), 2);
        assert_eq!(response.thinking[0]["signature"], "sig-abc");
        assert_eq!(response.thinking[1]["type"], "redacted_thinking");

        // Feed it back the way an agent round does.
        let req = ChatRequest {
            model: "claude-sonnet-5".into(),
            messages: vec![ChatMessage::Assistant {
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
                thinking: response.thinking.clone(),
            }],
            cache: CacheHint::Off,
            ..Default::default()
        };
        let blocks = provider().build_body(&req, false)["messages"][0]["content"]
            .as_array()
            .unwrap()
            .clone();
        let kinds: Vec<&str> = blocks
            .iter()
            .map(|b| b["type"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(kinds, ["thinking", "redacted_thinking", "text", "tool_use"]);
        // Verbatim, signature included — not reconstructed.
        assert_eq!(blocks[0]["signature"], "sig-abc");
        assert_eq!(blocks[0]["thinking"], "let me check");
        assert_eq!(blocks[1]["data"], "opaque");
    }

    #[test]
    fn streamed_thinking_reassembles_text_and_signature() {
        let mut assembler = StreamAssembler::default();
        assembler.handle(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        );
        assembler.handle(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"first "}}"#,
        );
        assembler.handle(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"second"}}"#,
        );
        assembler.handle(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"signature_delta","signature":"sig-xyz"}}"#,
        );
        assembler.handle(
            "content_block_start",
            r#"{"index":1,"content_block":{"type":"text","text":""}}"#,
        );
        assembler.handle(
            "content_block_delta",
            r#"{"index":1,"delta":{"type":"text_delta","text":"done"}}"#,
        );
        let events = assembler.handle("message_stop", "{}");
        let StreamEvent::Completed(response) = events[0].as_ref().unwrap() else {
            panic!("expected Completed");
        };
        assert_eq!(response.thinking.len(), 1);
        assert_eq!(response.thinking[0]["thinking"], "first second");
        // Without the signature the block is unusable on replay.
        assert_eq!(response.thinking[0]["signature"], "sig-xyz");
        assert_eq!(response.content.as_deref(), Some("done"));
    }

    #[test]
    fn caching_marks_the_system_prompt_and_the_conversation_tail() {
        let req = ChatRequest {
            model: "claude-sonnet-5".into(),
            system: "you are a scout".into(),
            messages: conversation(),
            ..Default::default()
        };
        let body = provider().build_body(&req, false);

        // System renders as a block array so it has somewhere to carry the
        // marker; the plain-string form cannot.
        assert_eq!(body["system"][0]["text"], "you are a scout");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");

        // The tail marker lands on the final tool result, so the next round
        // reads this whole conversation back.
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        let last_block = last["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last_block["type"], "tool_result");
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");

        // Anthropic permits four; we use two and must stay well under.
        assert_eq!(breakpoints(&body).len(), 2);
    }

    #[test]
    fn cache_hint_off_sends_no_breakpoints() {
        let req = ChatRequest {
            model: "claude-sonnet-5".into(),
            system: "you are a scout".into(),
            messages: conversation(),
            cache: CacheHint::Off,
            ..Default::default()
        };
        let body = provider().build_body(&req, false);
        assert!(breakpoints(&body).is_empty());
        // Off keeps the original plain-string system shape.
        assert_eq!(body["system"], "you are a scout");
    }

    #[test]
    fn a_lone_user_message_is_promoted_to_a_block_so_it_can_be_marked() {
        // Round one of an agent loop: the task prompt is the biggest thing in
        // the request and there is no tail yet. Marking it is what lets round
        // two read it back rather than re-sending it.
        let req = ChatRequest {
            model: "claude-sonnet-5".into(),
            messages: vec![ChatMessage::User {
                content: "a very large task brief".into(),
            }],
            ..Default::default()
        };
        let body = provider().build_body(&req, false);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "text");
        assert_eq!(block["text"], "a very large task brief");
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn caching_tolerates_an_empty_assistant_turn() {
        // No text and no tool calls means no block to mark. One fewer
        // breakpoint is fine; a malformed request is not.
        let req = ChatRequest {
            model: "claude-sonnet-5".into(),
            messages: vec![ChatMessage::Assistant {
                content: None,
                thinking: Vec::new(),
                tool_calls: Vec::new(),
            }],
            ..Default::default()
        };
        let body = provider().build_body(&req, false);
        assert!(body["messages"][0]["content"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(breakpoints(&body).is_empty());
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let messages = vec![
            ChatMessage::Assistant {
                content: None,
                thinking: Vec::new(),
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

    #[test]
    fn parses_cache_token_counts() {
        let value = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7,
                "cache_creation_input_tokens": 2048,
                "cache_read_input_tokens": 8192,
            },
        });
        let usage = parse_response(&value).unwrap().usage;
        assert_eq!(usage.cache_creation_input_tokens, 2048);
        assert_eq!(usage.cache_read_input_tokens, 8192);
        // Anthropic reports the three input figures disjointly, so the
        // prompt's real size is their sum — not `input_tokens` alone.
        assert_eq!(usage.total_input_tokens(), 12 + 2048 + 8192);
    }

    #[test]
    fn cache_counts_are_zero_when_the_response_omits_them() {
        // The shape of every response today: no breakpoint sent, so the API
        // reports no cache fields at all. Absent must read as zero, not fail.
        let value = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 7},
        });
        let usage = parse_response(&value).unwrap().usage;
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.total_input_tokens(), 12);
    }

    #[test]
    fn stream_assembler_reads_cache_counts_from_message_start() {
        let mut assembler = StreamAssembler::default();
        assembler.handle(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":5,"cache_read_input_tokens":4096,
               "cache_creation_input_tokens":128}}}"#,
        );
        assembler.handle(
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
        );
        let events = assembler.handle("message_stop", "{}");
        let StreamEvent::Completed(response) = events[0].as_ref().unwrap() else {
            panic!("expected Completed");
        };
        assert_eq!(response.usage.cache_read_input_tokens, 4096);
        assert_eq!(response.usage.cache_creation_input_tokens, 128);
        // `message_delta` revises output without clobbering the input side.
        assert_eq!(response.usage.input_tokens, 5);
        assert_eq!(response.usage.output_tokens, 9);
    }
}
