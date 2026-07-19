//! Cross-provider failover for provider outages.
//!
//! A [`FailoverProvider`] wraps a primary provider plus an ordered list of
//! fallback candidates (each its own provider + model). A call moves to the
//! next candidate only on outage-shaped errors — the same transient class
//! the retry layer recognizes (429/5xx/connect/timeout), and only after the
//! failing provider's own retries are exhausted. Permanent errors (4xx,
//! parse/schema failures) propagate immediately: a bad request would fail
//! everywhere, and masking it behind a second bill helps no one.
//!
//! Streaming follows the retry contract: failover happens only while
//! establishing the stream. Once a stream starts, mid-stream errors are
//! surfaced, not retried elsewhere.

use crate::retry::is_transient;
use crate::types::{ChatRequest, ChatResponse, EventStream};
use crate::{ChatProvider, LlmError};
use async_trait::async_trait;
use std::sync::Arc;

/// One failover candidate: where to send the retry and as which model.
pub(crate) struct Candidate {
    pub provider: Arc<dyn ChatProvider>,
    /// Config name of the provider, for the failover log line.
    pub provider_name: String,
    pub model: String,
    /// Overrides the request temperature when set; otherwise the request's
    /// effective temperature carries over.
    pub temperature: Option<f32>,
}

/// A [`ChatProvider`] that fails over across providers. The first request
/// goes to the primary untouched (the caller already applied the primary's
/// model/temperature); each fallback rewrites the request to its own model.
pub(crate) struct FailoverProvider {
    pub primary: Arc<dyn ChatProvider>,
    pub primary_name: String,
    pub fallbacks: Vec<Candidate>,
}

impl FailoverProvider {
    fn attempts(&self, req: &ChatRequest) -> Vec<(ChatRequest, &str, &Arc<dyn ChatProvider>)> {
        let mut attempts = vec![(req.clone(), self.primary_name.as_str(), &self.primary)];
        for candidate in &self.fallbacks {
            let mut attempt = req.clone();
            attempt.model = candidate.model.clone();
            attempt.temperature = candidate.temperature.or(attempt.temperature);
            attempts.push((
                attempt,
                candidate.provider_name.as_str(),
                &candidate.provider,
            ));
        }
        attempts
    }
}

#[async_trait]
impl ChatProvider for FailoverProvider {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut last: Option<LlmError> = None;
        for (attempt, name, provider) in self.attempts(&req) {
            if let Some(failed) = &last {
                tracing::warn!(
                    provider = name,
                    model = %attempt.model,
                    error = %failed,
                    "provider outage; failing over"
                );
            }
            match provider.chat(attempt).await {
                Ok(response) => return Ok(response),
                Err(error) if is_transient(&error) => last = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last.expect("at least the primary attempt ran"))
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let mut last: Option<LlmError> = None;
        for (attempt, name, provider) in self.attempts(&req) {
            if let Some(failed) = &last {
                tracing::warn!(
                    provider = name,
                    model = %attempt.model,
                    error = %failed,
                    "provider outage; failing over"
                );
            }
            match provider.chat_stream(attempt).await {
                Ok(stream) => return Ok(stream),
                Err(error) if is_transient(&error) => last = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last.expect("at least the primary attempt ran"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StopReason, StreamEvent, Usage};
    use futures::StreamExt;
    use std::sync::Mutex;

    /// Scripted provider: pops one outcome per call and records the
    /// requests it received.
    struct ScriptedProvider {
        outcomes: Mutex<Vec<Result<ChatResponse, LlmError>>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedProvider {
        fn new(outcomes: Vec<Result<ChatResponse, LlmError>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(outcomes),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChatProvider for ScriptedProvider {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.requests.lock().unwrap().push(req);
            self.outcomes.lock().unwrap().remove(0)
        }

        async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
            let response = self.chat(req).await?;
            Ok(futures::stream::once(async move { Ok(StreamEvent::Completed(response)) }).boxed())
        }
    }

    fn answer(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            tool_calls: Vec::new(),
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn outage() -> LlmError {
        LlmError::Api {
            status: 503,
            body: "overloaded".into(),
            retry_after: None,
        }
    }

    fn bad_request() -> LlmError {
        LlmError::Api {
            status: 400,
            body: "bad".into(),
            retry_after: None,
        }
    }

    fn failover(
        primary: Arc<ScriptedProvider>,
        fallbacks: Vec<(&str, &str, Option<f32>, Arc<ScriptedProvider>)>,
    ) -> FailoverProvider {
        FailoverProvider {
            primary,
            primary_name: "primary".into(),
            fallbacks: fallbacks
                .into_iter()
                .map(|(name, model, temperature, provider)| Candidate {
                    provider,
                    provider_name: name.into(),
                    model: model.into(),
                    temperature,
                })
                .collect(),
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "primary-model".into(),
            temperature: Some(0.2),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn fails_over_on_outage_and_rewrites_the_request() {
        let primary = ScriptedProvider::new(vec![Err(outage())]);
        let secondary = ScriptedProvider::new(vec![Ok(answer("from fallback"))]);
        let wrapper = failover(
            primary.clone(),
            vec![("backup", "backup-model", None, secondary.clone())],
        );

        let response = wrapper.chat(request()).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("from fallback"));
        // Primary saw the caller's request untouched.
        assert_eq!(primary.calls()[0].model, "primary-model");
        // Fallback got its own model; the effective temperature carried over.
        let fallback_req = &secondary.calls()[0];
        assert_eq!(fallback_req.model, "backup-model");
        assert_eq!(fallback_req.temperature, Some(0.2));
    }

    #[tokio::test]
    async fn fallback_temperature_overrides_when_set() {
        let primary = ScriptedProvider::new(vec![Err(outage())]);
        let secondary = ScriptedProvider::new(vec![Ok(answer("ok"))]);
        let wrapper = failover(
            primary,
            vec![("backup", "backup-model", Some(0.9), secondary.clone())],
        );
        wrapper.chat(request()).await.unwrap();
        assert_eq!(secondary.calls()[0].temperature, Some(0.9));
    }

    #[tokio::test]
    async fn permanent_errors_do_not_fail_over() {
        let primary = ScriptedProvider::new(vec![Err(bad_request())]);
        let secondary = ScriptedProvider::new(vec![Ok(answer("never"))]);
        let wrapper = failover(primary, vec![("backup", "m", None, secondary.clone())]);

        let error = wrapper.chat(request()).await.unwrap_err();
        assert!(matches!(error, LlmError::Api { status: 400, .. }));
        assert!(secondary.calls().is_empty(), "fallback must not be called");
    }

    #[tokio::test]
    async fn exhausted_chain_returns_the_last_outage() {
        let primary = ScriptedProvider::new(vec![Err(outage())]);
        let secondary = ScriptedProvider::new(vec![Err(LlmError::Api {
            status: 529,
            body: "also down".into(),
            retry_after: None,
        })]);
        let wrapper = failover(primary, vec![("backup", "m", None, secondary)]);

        let error = wrapper.chat(request()).await.unwrap_err();
        assert!(matches!(error, LlmError::Api { status: 529, .. }));
    }

    #[tokio::test]
    async fn tries_fallbacks_in_order() {
        let primary = ScriptedProvider::new(vec![Err(outage())]);
        let second = ScriptedProvider::new(vec![Err(outage())]);
        let third = ScriptedProvider::new(vec![Ok(answer("third"))]);
        let wrapper = failover(
            primary,
            vec![
                ("second", "m2", None, second.clone()),
                ("third", "m3", None, third.clone()),
            ],
        );

        let response = wrapper.chat(request()).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("third"));
        assert_eq!(second.calls().len(), 1);
        assert_eq!(third.calls()[0].model, "m3");
    }

    #[tokio::test]
    async fn streaming_fails_over_before_the_stream_starts() {
        let primary = ScriptedProvider::new(vec![Err(outage())]);
        let secondary = ScriptedProvider::new(vec![Ok(answer("streamed"))]);
        let wrapper = failover(primary, vec![("backup", "backup-model", None, secondary)]);

        let mut stream = wrapper.chat_stream(request()).await.unwrap();
        match stream.next().await.unwrap().unwrap() {
            StreamEvent::Completed(response) => {
                assert_eq!(response.content.as_deref(), Some("streamed"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
