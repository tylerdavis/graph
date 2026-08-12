//! Token metering: one choke point every billable call passes through.
//!
//! [`MeteredProvider`] is a [`ChatProvider`] decorator that reports each
//! call's token counts to a [`UsageMeter`] the consumer supplies. The
//! [`ModelRouter`](crate::ModelRouter) installs it around the primary provider
//! *and* each failover candidate individually, so a call that fails over is
//! attributed to the model that actually served it rather than the one that
//! was asked for.
//!
//! Metering here rather than at each call site catches every inference in the
//! workspace — planner, solver, repair, judge gates, agent rounds, prompt
//! tools, drafting — including the two that are otherwise invisible: the
//! structured-output repair pass and every failover attempt.
//!
//! **Known under-count.** Both providers can re-POST a fully valid request
//! inside a single `chat()` call — Anthropic when it strips an unsupported
//! `temperature`, OpenAI-compat when it falls back from `json_schema` to
//! `json_object`. Only the successful POST reports usage, so those rare paths
//! bill more than they meter. Fixing it would mean threading a meter through
//! each provider's internal retry loop, which is not worth the coupling.

use crate::types::{ChatRequest, ChatResponse, EventStream, StreamEvent, Usage};
use crate::{ChatProvider, LlmError};
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One completed model call, as the meter sees it.
#[derive(Debug, Clone)]
pub struct LlmCall {
    /// Config name of the provider that served the call.
    pub provider: String,
    /// The model as sent on the wire — after any failover rewrite.
    pub model: String,
    pub usage: Usage,
    pub elapsed: Duration,
}

/// Receives every metered call. Implementors aggregate; they must not fail —
/// a bookkeeping problem never breaks an inference.
pub trait UsageMeter: Send + Sync {
    fn record(&self, call: LlmCall);
}

/// Wraps a provider and reports each call's usage to a meter.
pub struct MeteredProvider {
    inner: Arc<dyn ChatProvider>,
    provider: String,
    meter: Arc<dyn UsageMeter>,
}

impl MeteredProvider {
    pub fn new(inner: Arc<dyn ChatProvider>, provider: String, meter: Arc<dyn UsageMeter>) -> Self {
        Self {
            inner,
            provider,
            meter,
        }
    }

    fn record(&self, model: String, usage: Usage, started: Instant) {
        self.meter.record(LlmCall {
            provider: self.provider.clone(),
            model,
            usage,
            elapsed: started.elapsed(),
        });
    }
}

#[async_trait]
impl ChatProvider for MeteredProvider {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let started = Instant::now();
        let model = req.model.clone();
        let response = self.inner.chat(req).await?;
        self.record(model, response.usage, started);
        Ok(response)
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let started = Instant::now();
        let model = req.model.clone();
        let stream = self.inner.chat_stream(req).await?;

        // Usage only lands on the terminal `Completed` event, so the meter
        // has to ride the stream rather than the call that opened it. A
        // stream that errors or is dropped early reports nothing — the
        // tokens are real but unknowable from here.
        let provider = self.provider.clone();
        let meter = self.meter.clone();
        Ok(stream
            .inspect(move |event| {
                if let Ok(StreamEvent::Completed(response)) = event {
                    meter.record(LlmCall {
                        provider: provider.clone(),
                        model: model.clone(),
                        usage: response.usage,
                        elapsed: started.elapsed(),
                    });
                }
            })
            .boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StopReason;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<LlmCall>>);

    impl Recorder {
        fn calls(&self) -> Vec<LlmCall> {
            self.0.lock().unwrap().clone()
        }
    }

    impl UsageMeter for Recorder {
        fn record(&self, call: LlmCall) {
            self.0.lock().unwrap().push(call);
        }
    }

    struct Fixed(Usage);

    #[async_trait]
    impl ChatProvider for Fixed {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                content: Some(format!("answered as {}", req.model)),
                tool_calls: Vec::new(),
                thinking: Vec::new(),
                structured: None,
                stop_reason: StopReason::EndTurn,
                usage: self.0,
            })
        }

        async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
            let response = self.chat(req).await?;
            Ok(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta("hi".into())),
                Ok(StreamEvent::Completed(response)),
            ])
            .boxed())
        }
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    fn request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn records_a_non_streaming_call() {
        let meter = Arc::new(Recorder::default());
        let provider = MeteredProvider::new(
            Arc::new(Fixed(usage(100, 20))),
            "anthropic".into(),
            meter.clone(),
        );

        provider.chat(request("claude-sonnet-5")).await.unwrap();

        let calls = meter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].provider, "anthropic");
        assert_eq!(calls[0].model, "claude-sonnet-5");
        assert_eq!(calls[0].usage.input_tokens, 100);
        assert_eq!(calls[0].usage.output_tokens, 20);
    }

    #[tokio::test]
    async fn records_a_streaming_call_once_the_stream_completes() {
        let meter = Arc::new(Recorder::default());
        let provider = MeteredProvider::new(
            Arc::new(Fixed(usage(7, 3))),
            "anthropic".into(),
            meter.clone(),
        );

        let mut stream = provider.chat_stream(request("m")).await.unwrap();
        // Opening the stream must not record — the counts aren't known yet.
        assert!(meter.calls().is_empty());

        while stream.next().await.is_some() {}
        let calls = meter.calls();
        assert_eq!(calls.len(), 1, "exactly one record per completed stream");
        assert_eq!(calls[0].usage.input_tokens, 7);
    }

    #[tokio::test]
    async fn an_abandoned_stream_records_nothing() {
        let meter = Arc::new(Recorder::default());
        let provider =
            MeteredProvider::new(Arc::new(Fixed(usage(7, 3))), "p".into(), meter.clone());

        let mut stream = provider.chat_stream(request("m")).await.unwrap();
        stream.next().await; // the TextDelta, then walk away
        drop(stream);

        assert!(meter.calls().is_empty());
    }
}
