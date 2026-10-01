use super::EventSink;
use crate::usage::CallSite;
use async_trait::async_trait;
use graph_llm::types::{ChatMessage, ChatRequest, ResponseSchema, StopReason, ToolCall, ToolSpec};
use graph_llm::ModelRouter;
use serde::Serialize;
use serde_json::{json, Value};

const FINAL_ROUND_NOTICE: &str = include_str!("prompts/final_round_notice.md").trim_ascii_end();

const FINAL_ROUND_NOTICE_TEXT: &str =
    include_str!("prompts/final_round_notice_text.md").trim_ascii_end();

#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub model: String,
    pub system: String,
    pub prompt: String,
    pub tools: Vec<ToolSpec>,
    pub max_iterations: Option<u32>,
    pub output_schema: Option<Value>,
}

/// One tool call inside the agent loop, for the call log.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallEntry {
    pub tool: String,
    pub round: u32,
}

#[derive(Debug)]
pub struct TaskOutcome {
    pub output: Value,
    pub iterations: u32,
    pub tools_called: Vec<ToolCallEntry>,
    pub final_: bool,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug)]
pub enum TaskError {
    Failed(String),
    Aborted(Option<Value>),
}

#[async_trait]
pub trait TaskTools: Send + Sync {
    async fn execute(&self, calls: &[ToolCall], round: u32) -> Result<Vec<ChatMessage>, TaskError>;
}

fn finish(
    output: Value,
    iterations: u32,
    tools_called: &[ToolCallEntry],
    final_: bool,
    messages: &[ChatMessage],
) -> Result<TaskOutcome, TaskError> {
    Ok(TaskOutcome {
        output,
        iterations,
        tools_called: tools_called.to_vec(),
        final_,
        messages: messages.to_vec(),
    })
}

pub async fn run_task_agent(
    spec: &TaskSpec,
    router: &ModelRouter,
    site: &CallSite,
    events: &dyn EventSink,
    tools: &dyn TaskTools,
) -> Result<TaskOutcome, TaskError> {
    let mut messages: Vec<ChatMessage> = vec![ChatMessage::User {
        content: spec.prompt.clone(),
    }];
    let mut tools_called: Vec<ToolCallEntry> = Vec::new();
    let mut round: u32 = 0;

    loop {
        if spec.max_iterations.is_some_and(|max| round >= max) {
            // Unreachable in practice: the final round below withdraws
            // tools and forces an answer, so the loop returns from there.
            // Kept as a backstop for a final round that produced neither
            // structured output nor text.
            return finish(json!({}), round, &tools_called, false, &messages);
        }
        round += 1;
        // Same signal the ask/chat loop emits between rounds: a long
        // agent step is otherwise silent between its step events.
        if round > 1 {
            events.iteration(round);
        }

        // The last round is the answer round. Withdrawing the tools and
        // setting `response_schema` makes the provider *force* a
        // conforming object (Anthropic does it with a synthetic tool and
        // `tool_choice`), so a budget that runs out yields a partial
        // answer instead of the empty result it used to. That forcing is
        // also why tools cannot stay on: a forced tool_choice would make
        // them unreachable anyway.
        let final_round = spec.max_iterations == Some(round);
        if final_round {
            let notice = match spec.output_schema {
                Some(_) => FINAL_ROUND_NOTICE,
                None => FINAL_ROUND_NOTICE_TEXT,
            };
            messages.push(ChatMessage::User {
                content: notice.to_string(),
            });
        }

        let request = ChatRequest {
            model: spec.model.clone(),
            system: spec.system.clone(),
            messages: messages.clone(),
            tools: if final_round {
                Vec::new()
            } else {
                spec.tools.clone()
            },
            response_schema: if final_round || spec.tools.is_empty() {
                spec.output_schema.clone().map(|schema| ResponseSchema {
                    name: "agent_output".to_string(),
                    schema,
                })
            } else {
                None
            },
            ..Default::default()
        };

        // Retries and cross-provider failover live in graph-llm, under
        // every provider call — transparent here, and they never
        // consume a round.
        let response = site
            .clone()
            .scope(router.chat_named(&spec.model, request))
            .await
            .map_err(|e| TaskError::Failed(format!("LLM call failed: {e}")))?;

        messages.push(ChatMessage::Assistant {
            content: response.content.clone(),
            tool_calls: response.tool_calls.clone(),
            // Carried forward so the model keeps its own
            // reasoning across rounds instead of re-deriving it.
            thinking: response.thinking.clone(),
        });

        // The forced final round answers through `structured`, not text.
        // It is already schema-validated by the provider, so there is
        // nothing to parse and no repair pass to pay for.
        if let Some(output) = response.structured {
            return finish(output, round, &tools_called, true, &messages);
        }

        if response.tool_calls.is_empty() {
            let text = response.content.unwrap_or_default();
            if text.trim().is_empty() {
                if response.stop_reason == StopReason::MaxTokens {
                    return Err(TaskError::Failed(
                        "model hit output-token limit without producing text or tool calls".into(),
                    ));
                }
                let nudge = match spec.output_schema {
                    Some(_) => {
                        "Provide your answer as JSON matching the output schema, \
                         or call tools to gather what you still need."
                    }
                    None => "Provide your answer, or call tools to gather what you still need.",
                };
                messages.push(ChatMessage::User {
                    content: nudge.to_string(),
                });
                continue;
            }

            let Some(schema) = &spec.output_schema else {
                return finish(
                    json!({ "result": text }),
                    round,
                    &tools_called,
                    true,
                    &messages,
                );
            };
            match site
                .clone()
                .scope(parse_and_validate_structured_output(&text, schema, router))
                .await
            {
                Ok(output) => return finish(output, round, &tools_called, true, &messages),
                Err(problem) => {
                    messages.push(ChatMessage::User {
                        content: format!(
                            "Your output did not match the required schema: {problem}\n\n\
                             Reply with corrected JSON only."
                        ),
                    });
                    continue;
                }
            }
        }

        for call in &response.tool_calls {
            tools_called.push(ToolCallEntry {
                tool: call.name.clone(),
                round,
            });
        }
        let results = tools.execute(&response.tool_calls, round).await?;
        messages.extend(results);
    }
}

/// Try to parse text as JSON and validate against the output schema.
/// On failure, attempt one repair pass.
/// Returns Ok(output) on success, Err(error_message) on failure.
async fn parse_and_validate_structured_output(
    text: &str,
    schema: &Value,
    router: &ModelRouter,
) -> Result<Value, String> {
    let json_value: Value = extract_json(text)
        .ok_or_else(|| format!("output is not valid JSON: {}", truncate_for_error(text)))?;

    // Validate against schema
    let validator =
        jsonschema::validator_for(schema).map_err(|e| format!("invalid output schema: {e}"))?;

    let errors: Vec<String> = validator
        .iter_errors(&json_value)
        .map(|e| e.to_string())
        .collect();
    if errors.is_empty() {
        return Ok(json_value);
    }

    // Attempt repair
    let error_msg = errors.join("; ");
    let repaired = router
        .repair_structured(&json_value, schema, &error_msg)
        .await
        .map_err(|e| format!("output repair failed: {e}"))?;

    // Re-validate repaired output
    let remaining: Vec<String> = validator
        .iter_errors(&repaired)
        .map(|e| e.to_string())
        .collect();
    if remaining.is_empty() {
        Ok(repaired)
    } else {
        Err(format!(
            "output still does not match schema after repair: {}",
            remaining.join("; ")
        ))
    }
}

/// Pull a JSON object out of a model's text answer.
///
/// Provider-native structured output is unavailable here: Anthropic
/// enforces a schema by *forcing* a synthetic tool call, which would end
/// the agent's loop on round one. So the schema is carried in the system
/// prompt and the answer arrives as prose — which real models routinely
/// wrap in a ```json fence or precede with a sentence. Tolerate both
/// rather than burning an iteration on formatting.
fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }

    // Fenced block, with or without a language tag.
    if let Some(rest) = trimmed.strip_prefix("```") {
        let body = rest.split_once('\n').map(|(_tag, b)| b).unwrap_or(rest);
        let body = body.strip_suffix("```").unwrap_or(body);
        if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
            return Some(value);
        }
    }

    // Preamble/postamble around a bare object: take the outermost braces.
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end > start {
        if let Ok(value) = serde_json::from_str::<Value>(&trimmed[start..=end]) {
            return Some(value);
        }
    }
    None
}

/// Model output is unbounded; keep it out of error messages at full length.
fn truncate_for_error(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= 200 {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(200).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::NullSink;
    use graph_config::{ModelChoice, ModelRoles};
    use graph_llm::types::{ChatResponse, EventStream, StreamEvent, Usage};
    use graph_llm::{ChatProvider, LlmError};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct Scripted {
        responses: Mutex<Vec<ChatResponse>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait]
    impl ChatProvider for Scripted {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.requests.lock().unwrap().push(req);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(LlmError::Parse("script exhausted".into()));
            }
            Ok(responses.remove(0))
        }

        async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
            use futures::StreamExt;
            let response = self.chat(req).await?;
            Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(response))]).boxed())
        }
    }

    fn response(content: Option<&str>, tool_calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            content: content.map(str::to_string),
            tool_calls,
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "t__echo".to_string(),
            arguments: json!({}),
        }
    }

    fn router(responses: Vec<ChatResponse>) -> (ModelRouter, Arc<Scripted>) {
        let provider = Arc::new(Scripted {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        });
        let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
        providers.insert("mock".to_string(), provider.clone());
        let roles = ModelRoles::from([(
            "default",
            ModelChoice {
                provider: "mock".to_string(),
                model: "test".to_string(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
            },
        )]);
        (ModelRouter::with_providers(providers, roles), provider)
    }

    struct EchoTools;

    #[async_trait]
    impl TaskTools for EchoTools {
        async fn execute(
            &self,
            calls: &[ToolCall],
            _round: u32,
        ) -> Result<Vec<ChatMessage>, TaskError> {
            Ok(calls
                .iter()
                .map(|call| ChatMessage::ToolResult {
                    tool_call_id: call.id.clone(),
                    content: json!({"ok": true}),
                    is_error: false,
                })
                .collect())
        }
    }

    fn spec(max_iterations: Option<u32>, output_schema: Option<Value>) -> TaskSpec {
        TaskSpec {
            model: "chat".to_string(),
            system: "system".to_string(),
            prompt: "do the thing".to_string(),
            tools: vec![ToolSpec {
                name: "t__echo".to_string(),
                description: "echo".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            max_iterations,
            output_schema,
        }
    }

    #[tokio::test]
    async fn without_an_output_schema_the_final_text_is_the_result() {
        let (router, provider) = router(vec![
            response(None, vec![call("a")]),
            response(Some("the answer"), vec![]),
        ]);
        let outcome = run_task_agent(
            &spec(None, None),
            &router,
            &CallSite::role("chat"),
            &NullSink,
            &EchoTools,
        )
        .await
        .unwrap();
        assert_eq!(outcome.output, json!({"result": "the answer"}));
        assert!(outcome.final_);
        assert_eq!(outcome.iterations, 2);
        assert_eq!(outcome.tools_called.len(), 1);
        let requests = provider.requests.lock().unwrap();
        assert!(requests.iter().all(|r| r.response_schema.is_none()));
        assert_eq!(
            requests[0].messages.len(),
            1,
            "the prompt is the only user turn"
        );
    }

    #[tokio::test]
    async fn without_a_round_cap_the_loop_runs_until_the_model_answers() {
        let mut script: Vec<ChatResponse> = (0..12)
            .map(|i| response(None, vec![call(&format!("c{i}"))]))
            .collect();
        script.push(response(Some("done"), vec![]));
        let (router, provider) = router(script);
        let outcome = run_task_agent(
            &spec(None, None),
            &router,
            &CallSite::role("chat"),
            &NullSink,
            &EchoTools,
        )
        .await
        .unwrap();
        assert_eq!(outcome.iterations, 13);
        assert_eq!(outcome.output, json!({"result": "done"}));
        let requests = provider.requests.lock().unwrap();
        assert!(
            requests.iter().all(|r| !r.tools.is_empty()),
            "tools are never withdrawn"
        );
    }

    #[tokio::test]
    async fn a_capped_run_without_a_schema_withdraws_tools_on_the_final_round() {
        let (router, provider) = router(vec![
            response(None, vec![call("a")]),
            response(Some("partial"), vec![]),
        ]);
        let outcome = run_task_agent(
            &spec(Some(2), None),
            &router,
            &CallSite::role("chat"),
            &NullSink,
            &EchoTools,
        )
        .await
        .unwrap();
        assert_eq!(outcome.output, json!({"result": "partial"}));
        let requests = provider.requests.lock().unwrap();
        let last = requests.last().unwrap();
        assert!(last.tools.is_empty());
        assert!(last.response_schema.is_none());
        let ChatMessage::User { content } = last.messages.last().unwrap() else {
            panic!("the final notice is the last turn");
        };
        assert_eq!(content, FINAL_ROUND_NOTICE_TEXT);
    }

    #[test]
    fn extract_json_tolerates_what_real_models_actually_emit() {
        let want = json!({"found": 1});

        // Bare object.
        assert_eq!(extract_json(r#"{"found": 1}"#), Some(want.clone()));
        // Fenced with a language tag (the common Anthropic shape).
        assert_eq!(
            extract_json("```json\n{\"found\": 1}\n```"),
            Some(want.clone())
        );
        // Fenced without a tag.
        assert_eq!(extract_json("```\n{\"found\": 1}\n```"), Some(want.clone()));
        // Conversational preamble.
        assert_eq!(
            extract_json("Here is the result:\n\n{\"found\": 1}"),
            Some(want.clone())
        );
        // Preamble AND a fence AND a trailing remark.
        assert_eq!(
            extract_json("Sure!\n```json\n{\"found\": 1}\n```\nLet me know."),
            Some(want)
        );
        // Genuinely not JSON stays a failure.
        assert_eq!(extract_json("I could not complete the task."), None);
        assert_eq!(extract_json(""), None);
    }

    #[test]
    fn parse_failure_message_does_not_dump_the_whole_answer() {
        let long = "x".repeat(5000);
        let message = truncate_for_error(&long);
        assert!(message.chars().count() <= 201, "{}", message.len());
        assert!(message.ends_with('…'));
    }
}
