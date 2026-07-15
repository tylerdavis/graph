//! The ReAct agent loop: model ↔ tools until a final text answer.

mod events;

pub use events::{EventSink, NullSink};

use crate::tools::{ToolOutcome, ToolRegistry};
use futures::StreamExt;
use graph_llm::types::{
    ChatMessage, ChatRequest, ChatResponse, StopReason, StreamEvent, ToolCall, ToolSpec, Usage,
};
use graph_llm::ChatProvider;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

pub struct Agent {
    pub provider: Arc<dyn ChatProvider>,
    pub registry: Arc<dyn ToolRegistry>,
    pub events: Arc<dyn EventSink>,
    pub model: String,
    pub temperature: Option<f32>,
    pub system_prompt: String,
    pub max_iterations: u32,
}

/// Failures of the agent loop (`ask`/`chat`/workbench turns). The plan
/// pipeline never runs the agent loop, so these sit outside its
/// `EmptyData`/`BadPath` error taxonomy: they surface directly to the user
/// as a failed turn (rolled back in the workbench and `chat`, nonzero exit
/// in `ask`) and never trigger replanning.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] graph_llm::LlmError),
    #[error("agent exceeded {0} tool iterations without reaching an answer")]
    MaxIterations(u32),
    #[error(
        "the model hit its output-token limit before producing any visible text \
         (the whole budget went to internal reasoning) — raise max_tokens or \
         simplify the request"
    )]
    MaxTokensNoOutput,
    #[error("model stream ended without completing a response")]
    IncompleteStream,
}

#[derive(Debug)]
pub struct TurnOutcome {
    pub text: String,
    pub usage: Usage,
    pub tool_calls_made: u32,
    /// Names of tools invoked this turn, in first-use order, deduplicated.
    pub tools_used: Vec<String>,
}

impl Agent {
    /// Run one user turn: loop model → tools → model until the model answers
    /// with text and no tool calls. Appends every intermediate message to
    /// `messages` so the caller owns the full history.
    pub async fn run_turn(
        &self,
        messages: &mut Vec<ChatMessage>,
    ) -> Result<TurnOutcome, AgentError> {
        let tools = self.tool_specs().await;
        let mut usage = Usage::default();
        let mut tool_calls_made = 0u32;
        let mut tools_used: Vec<String> = Vec::new();

        for iteration in 0..self.max_iterations {
            if iteration > 0 {
                self.events.iteration(iteration);
            }
            let response = self.stream_once(messages, &tools).await?;
            usage.input_tokens += response.usage.input_tokens;
            usage.output_tokens += response.usage.output_tokens;

            messages.push(ChatMessage::Assistant {
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
            });

            if response.tool_calls.is_empty() {
                let text = response.content.unwrap_or_default();
                // A max_tokens stop with no text means the whole output
                // budget went to (invisible) thinking — silence here reads
                // as "nothing happened"; make it a diagnosable failure.
                if text.is_empty() && response.stop_reason == StopReason::MaxTokens {
                    return Err(AgentError::MaxTokensNoOutput);
                }
                return Ok(TurnOutcome {
                    text,
                    usage,
                    tool_calls_made,
                    tools_used,
                });
            }

            tool_calls_made += response.tool_calls.len() as u32;
            for call in &response.tool_calls {
                if !tools_used.contains(&call.name) {
                    tools_used.push(call.name.clone());
                }
            }
            let results = self.execute_calls(&response.tool_calls).await;
            messages.extend(results);
        }
        Err(AgentError::MaxIterations(self.max_iterations))
    }

    async fn tool_specs(&self) -> Vec<ToolSpec> {
        match self.registry.tools().await {
            Ok(defs) => defs
                .into_iter()
                .map(|def| ToolSpec {
                    name: def.name,
                    description: def.description,
                    input_schema: def.input_schema,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "tool discovery failed; running without tools");
                Vec::new()
            }
        }
    }

    async fn stream_once(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, AgentError> {
        let request = ChatRequest {
            model: self.model.clone(),
            system: self.system_prompt.clone(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            temperature: self.temperature,
            ..Default::default()
        };
        let mut stream = self.provider.chat_stream(request).await?;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta(text) => self.events.text_delta(&text),
                StreamEvent::ToolCallStarted { .. } => {}
                StreamEvent::Completed(response) => return Ok(response),
            }
        }
        Err(AgentError::IncompleteStream)
    }

    /// Execute one round of tool calls concurrently, preserving call order
    /// in the returned messages.
    async fn execute_calls(&self, calls: &[ToolCall]) -> Vec<ChatMessage> {
        let futures = calls.iter().map(|call| async {
            self.events.tool_started(&call.name, &call.arguments);
            let started = Instant::now();
            let outcome = self
                .registry
                .invoke(&call.name, call.arguments.clone())
                .await
                .unwrap_or_else(|e| ToolOutcome {
                    result: json!({ "error": e.to_string() }),
                    is_error: true,
                });
            self.events
                .tool_finished(&call.name, started.elapsed(), outcome.is_error);
            ChatMessage::ToolResult {
                tool_call_id: call.id.clone(),
                content: outcome.result,
                is_error: outcome.is_error,
            }
        });
        futures::future::join_all(futures).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolDef, ToolError};
    use async_trait::async_trait;
    use graph_llm::types::{EventStream, StopReason};
    use graph_llm::LlmError;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Provider that plays back a fixed sequence of responses.
    struct ScriptedProvider {
        responses: Mutex<Vec<ChatResponse>>,
        seen_requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                seen_requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for ScriptedProvider {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.seen_requests.lock().unwrap().push(req);
            Ok(self.responses.lock().unwrap().remove(0))
        }

        async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
            let response = {
                self.seen_requests.lock().unwrap().push(req);
                self.responses.lock().unwrap().remove(0)
            };
            let events = vec![Ok(StreamEvent::Completed(response))];
            Ok(futures::stream::iter(events).boxed())
        }
    }

    struct EchoRegistry {
        invocations: Mutex<Vec<(String, Value)>>,
        error_mode: bool,
    }

    #[async_trait]
    impl ToolRegistry for EchoRegistry {
        async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
            Ok(vec![ToolDef {
                name: "test__echo".into(),
                description: "echo".into(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            }])
        }

        async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
            self.invocations
                .lock()
                .unwrap()
                .push((name.to_string(), input.clone()));
            Ok(ToolOutcome {
                result: json!({"echoed": input}),
                is_error: self.error_mode,
            })
        }
    }

    fn agent(responses: Vec<ChatResponse>, error_mode: bool) -> (Agent, Arc<EchoRegistry>) {
        let registry = Arc::new(EchoRegistry {
            invocations: Mutex::new(Vec::new()),
            error_mode,
        });
        let agent = Agent {
            provider: Arc::new(ScriptedProvider::new(responses)),
            registry: registry.clone(),
            events: Arc::new(NullSink),
            model: "test-model".into(),
            temperature: None,
            system_prompt: "test".into(),
            max_iterations: 3,
        };
        (agent, registry)
    }

    fn text_response(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }
    }

    fn tool_response(calls: Vec<(&str, Value)>) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: calls
                .into_iter()
                .enumerate()
                .map(|(i, (name, arguments))| ToolCall {
                    id: format!("call-{i}"),
                    name: name.to_string(),
                    arguments,
                })
                .collect(),
            structured: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }
    }

    #[tokio::test]
    async fn answers_directly_without_tools() {
        let (agent, registry) = agent(vec![text_response("hi there")], false);
        let mut messages = vec![ChatMessage::User {
            content: "hello".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "hi there");
        assert_eq!(outcome.tool_calls_made, 0);
        assert_eq!(messages.len(), 2);
        assert!(registry.invocations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn max_tokens_with_no_text_is_a_diagnosable_error() {
        // The whole output budget went to invisible thinking: no tool
        // calls, no text, stop_reason max_tokens.
        let empty = ChatResponse {
            content: None,
            tool_calls: vec![],
            structured: None,
            stop_reason: StopReason::MaxTokens,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 8192,
            },
        };
        let (subject, _registry) = agent(vec![empty], false);
        let mut messages = vec![ChatMessage::User {
            content: "hello".into(),
        }];
        let err = subject.run_turn(&mut messages).await.unwrap_err();
        assert!(matches!(err, AgentError::MaxTokensNoOutput));
        assert!(err.to_string().contains("max_tokens"), "{err}");

        // A truncated-but-nonempty answer still returns normally.
        let mut truncated = text_response("partial answ");
        truncated.stop_reason = StopReason::MaxTokens;
        let (subject, _registry) = agent(vec![truncated], false);
        let mut messages = vec![ChatMessage::User {
            content: "hello".into(),
        }];
        let outcome = subject.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "partial answ");
    }

    #[tokio::test]
    async fn executes_tool_round_then_answers() {
        let (agent, registry) = agent(
            vec![
                tool_response(vec![("test__echo", json!({"q": "x"}))]),
                text_response("done"),
            ],
            false,
        );
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "done");
        assert_eq!(outcome.tool_calls_made, 1);
        assert_eq!(
            outcome.usage.input_tokens, 20,
            "usage accumulates across rounds"
        );
        // History: user, assistant(tool_calls), tool result, assistant(text).
        assert_eq!(messages.len(), 4);
        let invocations = registry.invocations.lock().unwrap();
        assert_eq!(
            invocations[0],
            ("test__echo".to_string(), json!({"q": "x"}))
        );
        assert!(matches!(
            &messages[2],
            ChatMessage::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn parallel_calls_produce_ordered_results() {
        let (agent, _) = agent(
            vec![
                tool_response(vec![
                    ("test__echo", json!({"n": 1})),
                    ("test__echo", json!({"n": 2})),
                ]),
                text_response("both"),
            ],
            false,
        );
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        agent.run_turn(&mut messages).await.unwrap();
        let ids: Vec<_> = messages
            .iter()
            .filter_map(|m| match m {
                ChatMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call-0", "call-1"]);
    }

    #[tokio::test]
    async fn tool_errors_flow_back_as_error_results() {
        let (agent, _) = agent(
            vec![
                tool_response(vec![("test__echo", json!({}))]),
                text_response("recovered"),
            ],
            true,
        );
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "recovered");
        assert!(matches!(
            &messages[2],
            ChatMessage::ToolResult { is_error: true, .. }
        ));
    }

    #[tokio::test]
    async fn max_iterations_stops_runaway_loops() {
        let responses = (0..3)
            .map(|_| tool_response(vec![("test__echo", json!({}))]))
            .collect();
        let (agent, _) = agent(responses, false);
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let err = agent.run_turn(&mut messages).await.unwrap_err();
        assert!(matches!(err, AgentError::MaxIterations(3)));
    }
}
