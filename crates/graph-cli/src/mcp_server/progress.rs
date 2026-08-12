//! Streaming progress from a running plan back to the MCP client.
//!
//! A plan can take minutes, and many MCP clients time a tool call out after
//! sixty seconds of silence. Progress notifications are how a long plan says
//! it is still working — and, incidentally, how a human watching the client
//! sees which step is running, the same thing `plan run` shows on a terminal.
//!
//! MCP makes this opt-in: the client sends a `progressToken` in the call's
//! `_meta`, and only then may the server notify against it. No token means no
//! notifications, which is why this is wired through an `Option`.
//!
//! The sink is called from the pipeline's synchronous event path, but sending
//! is async, so events are pushed onto an unbounded channel and drained by a
//! forwarding task. A plan's step count is not known up front, so `total` is
//! left unset and `progress` simply counts events — that is exactly the
//! "indeterminate but alive" case the field is specified for.

use graph_core::usage::UsageReport;
use graph_core::{compact_tokens, EventSink};
use graph_llm::types::Usage;
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::service::Peer;
use rmcp::RoleServer;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// The progress channel for one plan call.
///
/// Held by the caller for the duration of the run and then [`finish`]ed,
/// which closes the queue and waits for it to drain. Without that wait the
/// forwarding task races the tool reply, and a short plan would deliver its
/// notifications *after* the result they describe — or not at all.
///
/// [`finish`]: Progress::finish
pub struct Progress {
    sink: std::sync::Arc<dyn EventSink>,
    forwarder: Option<tokio::task::JoinHandle<()>>,
}

impl Progress {
    pub fn sink(&self) -> std::sync::Arc<dyn EventSink> {
        self.sink.clone()
    }

    /// Drop the sender and wait for every queued notification to go out.
    pub async fn finish(self) {
        let Progress { sink, forwarder } = self;
        drop(sink);
        if let Some(forwarder) = forwarder {
            let _ = forwarder.await;
        }
    }
}

/// The sink for one plan call: forwards progress when the client asked for
/// it, discards otherwise.
pub fn sink_for(context: &rmcp::service::RequestContext<RoleServer>) -> Progress {
    // rmcp lifts `_meta` off the request params into the request context, so
    // the token is read from there rather than from CallToolRequestParams.
    let token = context.meta.get_progress_token();
    match ProgressSink::new(context.peer.clone(), token) {
        Some((sink, forwarder)) => Progress {
            sink: std::sync::Arc::new(sink),
            forwarder: Some(forwarder),
        },
        None => Progress {
            sink: std::sync::Arc::new(graph_core::NullSink),
            forwarder: None,
        },
    }
}

/// An [`EventSink`] that turns pipeline events into MCP progress notifications.
pub struct ProgressSink {
    tx: UnboundedSender<String>,
    counter: AtomicU64,
    /// Running token totals, reported as the run proceeds.
    ///
    /// The final figure also reaches the caller on `plan_run`'s result body,
    /// so what this adds is the *mid-run* answer to "how much is this costing
    /// me" — which a client watching a twenty-minute review has no other way
    /// to get.
    spend: Mutex<(u64, Usage)>,
}

impl ProgressSink {
    /// Build a sink and spawn the task that forwards to the client.
    ///
    /// Returns `None` when the client did not ask for progress, so the caller
    /// falls back to a silent sink rather than doing bookkeeping nobody reads.
    pub fn new(
        peer: Peer<RoleServer>,
        token: Option<ProgressToken>,
    ) -> Option<(Self, tokio::task::JoinHandle<()>)> {
        let token = token?;
        let (tx, mut rx) = unbounded_channel::<String>();
        let forwarder = tokio::spawn(async move {
            let mut sent = 0.0;
            while let Some(message) = rx.recv().await {
                sent += 1.0;
                let mut param = ProgressNotificationParam::new(token.clone(), sent);
                param.message = Some(message);
                // A client that stopped listening is not a reason to fail the
                // run that is still producing useful work.
                if let Err(error) = peer.notify_progress(param).await {
                    tracing::debug!(%error, "progress notification dropped");
                    break;
                }
            }
        });
        Some((
            Self {
                tx,
                counter: AtomicU64::new(0),
                spend: Mutex::new((0, Usage::default())),
            },
            forwarder,
        ))
    }

    fn emit(&self, message: String) {
        self.counter.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(message);
    }
}

impl EventSink for ProgressSink {
    fn step_started(&self, call_stack: &[String], path: &str, tool: &str, _input: &Value) {
        // The call stack disambiguates a nested plan's E0 from the outer
        // plan's, which matters as soon as plans compose.
        let prefix = if call_stack.is_empty() {
            String::new()
        } else {
            format!("{} ▸ ", call_stack.join(" ▸ "))
        };
        self.emit(format!("{prefix}{path} {tool}"));
    }

    fn step_finished(
        &self,
        _call_stack: &[String],
        path: &str,
        tool: &str,
        _result: &Value,
        is_error: bool,
        elapsed: Duration,
    ) {
        if is_error {
            self.emit(format!("{path} {tool} failed after {elapsed:.1?}"));
        }
    }

    fn planning(&self) {
        self.emit("planning".into());
    }

    fn replanning(&self, attempt: u32) {
        self.emit(format!("replanning (attempt {attempt})"));
    }

    fn synthesizing(&self) {
        // Usually the longest single wait in a plan run, and the one most
        // likely to trip a client timeout with nothing else on the wire.
        self.emit("synthesizing the answer".into());
    }

    fn llm_call(&self, site: &str, _model: &str, usage: &Usage, _elapsed: Duration) {
        // Cumulative rather than per-call deltas: a client that samples the
        // notification stream still sees the current total, and one line per
        // inference sits at the same granularity as the step events already
        // on the wire.
        let (calls, total) = {
            let mut spend = self.spend.lock().unwrap();
            spend.0 += 1;
            spend.1.add(usage);
            (spend.0, spend.1)
        };
        self.emit(format!(
            "{site} — {calls} call(s), {} in / {} out so far",
            compact_tokens(total.total_input_tokens()),
            compact_tokens(total.output_tokens),
        ));
    }

    fn usage_summary(&self, report: &UsageReport) {
        self.emit(report.summary());
    }
    // text_delta / solver_delta are deliberately not forwarded: token-level
    // noise would be one notification per token.
}

/// Stops a run when the MCP client cancels the request.
///
/// MCP cancellation is a notification, not a reply — the client says "stop"
/// and stops caring. The pipeline already has the matching concept: an
/// [`ExecutionGate`] returning [`GateDecision::Abort`] halts before the next
/// real tool call and surfaces as `PipelineError::Aborted` carrying the
/// partial state, so a cancelled run stops *between* side effects rather
/// than in the middle of one.
///
/// The check is at the gate rather than a `select!` around the whole run for
/// exactly that reason: a plan that is midway through creating an issue
/// should finish that call and stop before the next, not be dropped.
pub struct CancelGate {
    token: tokio_util::sync::CancellationToken,
}

impl CancelGate {
    pub fn new(token: tokio_util::sync::CancellationToken) -> Self {
        Self { token }
    }
}

#[async_trait::async_trait]
impl graph_core::pipeline::ExecutionGate for CancelGate {
    async fn before_tool(
        &self,
        _ctx: graph_core::pipeline::GateContext<'_>,
    ) -> graph_core::pipeline::GateDecision {
        if self.token.is_cancelled() {
            graph_core::pipeline::GateDecision::Abort
        } else {
            graph_core::pipeline::GateDecision::Proceed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::pipeline::{ExecutionGate, GateDecision};

    /// A sink wired to a plain channel — `new` needs a live MCP peer, which a
    /// unit test has no business standing up.
    fn sink() -> (ProgressSink, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = unbounded_channel();
        (
            ProgressSink {
                tx,
                counter: AtomicU64::new(0),
                spend: Mutex::new((0, Usage::default())),
            },
            rx,
        )
    }

    fn tokens(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn spend_notifications_accumulate_across_calls() {
        let (sink, mut rx) = sink();

        sink.llm_call("E0", "m", &tokens(1_000, 100), Duration::from_millis(1));
        sink.llm_call("E1", "m", &tokens(2_000, 200), Duration::from_millis(1));

        let first = rx.try_recv().unwrap();
        let second = rx.try_recv().unwrap();
        assert!(
            first.starts_with("E0 — 1 call(s), 1k in / 100 out"),
            "{first}"
        );
        // Cumulative, not per-call: a client that sampled only this one line
        // still learns the run's total so far.
        assert!(
            second.starts_with("E1 — 2 call(s), 3k in / 300 out"),
            "{second}"
        );
    }

    #[tokio::test]
    async fn the_run_total_is_notified_at_the_end() {
        let (sink, mut rx) = sink();
        let report = graph_core::usage::UsageReport {
            calls: 3,
            total: tokens(5_000, 400),
            cost_usd: Some(1.5),
            ..Default::default()
        };

        sink.usage_summary(&report);

        assert_eq!(rx.try_recv().unwrap(), "3 calls · 5k in / 400 out · $1.50");
    }

    #[tokio::test]
    async fn the_gate_aborts_once_the_client_cancels() {
        use graph_core::pipeline::{GateContext, StepPath};
        use serde_json::json;

        let token = tokio_util::sync::CancellationToken::new();
        let gate = CancelGate::new(token.clone());
        let path = StepPath::top("E1");
        let input = json!({});
        let scope = serde_json::Map::new();
        let context = || GateContext {
            path: &path,
            tool_name: "t__search",
            rendered_input: &input,
            call_stack: &[],
            scope: &scope,
        };

        // Before cancellation the run proceeds untouched...
        assert!(matches!(
            gate.before_tool(context()).await,
            GateDecision::Proceed
        ));
        token.cancel();
        // ...and after it, the next tool call is the one that does not happen.
        assert!(matches!(
            gate.before_tool(context()).await,
            GateDecision::Abort
        ));
    }
}
