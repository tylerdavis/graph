//! OTLP export: one exporter for run traces and, optionally, the
//! diagnostic log, configured by `[telemetry]` and the standard `OTEL_*`
//! variables.
//!
//! Traces are built from the same [`EventSink`] events the terminal and
//! JSONL sinks render, teed alongside them by [`attach`]: the run is the
//! root span, each plan step or body call a child span keyed by its bus
//! path, every tool call a span under its step, and every billable model
//! call a `generation` under the step that spent it. Attributes follow the
//! OpenTelemetry GenAI conventions plus the `langfuse.*` keys Langfuse reads,
//! which other backends keep as metadata; there is no per-backend config.
//!
//! Process-wide state lives in a `OnceLock` because the sink is chosen per
//! command while the exporter must outlive every command and be flushed
//! once, from `main`, after the last event.

use graph_config::TelemetryConfig;
use graph_core::usage::{LlmCallEvent, UsageReport};
use graph_core::{EventSink, TeeSink};
use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanKind, Status, TraceContextExt, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue, Value as OtelValue};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_ATTRIBUTE_BYTES: usize = 64 * 1024;

static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();

struct Telemetry {
    tracer_provider: SdkTracerProvider,
    logger_provider: Option<SdkLoggerProvider>,
    tracer: SdkTracer,
    capture_content: bool,
    common: Vec<KeyValue>,
}

/// Outcome of [`init`]: whether the exporter is on, and anything the user
/// should hear about it once tracing is up (init runs before the log
/// subscriber exists, so it cannot warn itself).
#[derive(Default)]
pub struct Init {
    pub active: bool,
    pub warnings: Vec<String>,
}

/// Build the exporter from the given config layers, if `[telemetry]` (or
/// `OTEL_EXPORTER_OTLP_ENDPOINT`) names an endpoint. A config that fails to
/// load is left for the command to report; telemetry simply stays off.
pub fn init(config_paths: &[PathBuf]) -> Init {
    let mut init = Init::default();
    let Ok(loaded) = graph_config::load_from(config_paths) else {
        return init;
    };
    let telemetry = &loaded.config.telemetry;
    let resolved = match telemetry.with_env(&|name| std::env::var(name).ok()) {
        Ok(resolved) => resolved,
        Err(problem) => {
            init.warnings.push(format!("telemetry disabled: {problem}"));
            return init;
        }
    };
    if resolved.endpoint.is_none() {
        return init;
    }
    if !telemetry.missing_env.is_empty() {
        init.warnings.push(format!(
            "telemetry disabled: {}",
            graph_config::describe_missing_env("telemetry", &telemetry.missing_env)
        ));
        return init;
    }
    match build(&resolved) {
        Ok(built) => {
            init.active = TELEMETRY.set(built).is_ok();
        }
        Err(problem) => init
            .warnings
            .push(format!("telemetry disabled: {problem:#}")),
    }
    init
}

fn build(config: &TelemetryConfig) -> anyhow::Result<Telemetry> {
    let protocol = match config.protocol {
        graph_config::TelemetryProtocol::HttpProtobuf => Protocol::HttpBinary,
        graph_config::TelemetryProtocol::HttpJson => Protocol::HttpJson,
    };
    let headers: HashMap<String, String> = config
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let timeout = Duration::from_secs(config.timeout_secs());
    let resource = Resource::builder()
        .with_service_name(config.service_name().to_string())
        .with_attributes(
            std::iter::once(KeyValue::new("service.version", VERSION)).chain(
                config
                    .resource
                    .iter()
                    .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
            ),
        )
        .build();

    let traces_url = config
        .signal_url("v1/traces")
        .ok_or_else(|| anyhow::anyhow!("no telemetry endpoint"))?;
    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(protocol)
        .with_endpoint(traces_url)
        .with_headers(headers.clone())
        .with_timeout(timeout)
        .build()?;
    let span_processor =
        opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor::builder(
            span_exporter,
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();
    let tracer_provider = SdkTracerProvider::builder()
        .with_span_processor(span_processor)
        .with_resource(resource.clone())
        .build();
    let tracer = tracer_provider.tracer("graph");

    let logger_provider = if config.logs {
        let logs_url = config
            .signal_url("v1/logs")
            .ok_or_else(|| anyhow::anyhow!("no telemetry endpoint"))?;
        let log_exporter = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(protocol)
            .with_endpoint(logs_url)
            .with_headers(headers)
            .with_timeout(timeout)
            .build()?;
        let log_processor =
            opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor::builder(
                log_exporter,
                opentelemetry_sdk::runtime::Tokio,
            )
            .build();
        Some(
            SdkLoggerProvider::builder()
                .with_log_processor(log_processor)
                .with_resource(resource)
                .build(),
        )
    } else {
        None
    };

    let mut common = vec![KeyValue::new("langfuse.release", VERSION)];
    common.extend(
        config
            .resource
            .iter()
            .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
    );
    Ok(Telemetry {
        tracer_provider,
        logger_provider,
        tracer,
        capture_content: config.capture_content,
        common,
    })
}

/// Whether model calls should carry prompts and completions — what the
/// usage ledger asks its metered providers for.
pub fn captures_content() -> bool {
    TELEMETRY.get().is_some_and(|t| t.capture_content)
}

/// The `tracing` layer that exports the diagnostic stream as OTLP logs, or
/// `None` when `[telemetry].logs` is off. The exporter's own HTTP stack is
/// filtered out so its diagnostics can never feed back into itself.
pub fn logs_layer<S>() -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::Layer as _;
    let provider = TELEMETRY.get()?.logger_provider.as_ref()?;
    let bridge = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(provider);
    Some(
        bridge.with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
            let target = metadata.target();
            !["opentelemetry", "hyper", "reqwest", "h2", "rustls", "tower"]
                .iter()
                .any(|prefix| target.starts_with(prefix))
        })),
    )
}

/// Flush and stop the exporter. Called once from `main` after the command
/// returns, so it runs after every event, including the usage summary.
pub async fn shutdown() {
    let Some(telemetry) = TELEMETRY.get() else {
        return;
    };
    let _ = tokio::task::spawn_blocking(|| {
        if let Err(error) = telemetry.tracer_provider.shutdown() {
            tracing::debug!(%error, "telemetry trace exporter shutdown");
        }
        if let Some(logs) = &telemetry.logger_provider {
            if let Err(error) = logs.shutdown() {
                tracing::debug!(%error, "telemetry log exporter shutdown");
            }
        }
    })
    .await;
}

/// What a trace is about: the name it is filed under and the ids that
/// group it with its neighbours.
#[derive(Debug, Clone)]
pub struct RunInfo {
    /// The trace name: a plan identifier, `ask`, `chat`, `plan draft`.
    pub name: String,
    pub kind: RunKind,
    /// Groups traces into a session (Langfuse `session.id`): the thread id
    /// for conversations.
    pub session_id: Option<String>,
    /// The `[user].name`, when set.
    pub user_id: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    PlanRun,
    Conversation,
    Draft,
}

impl RunInfo {
    pub fn plan_run(identifier: &str) -> Self {
        Self {
            name: identifier.to_string(),
            kind: RunKind::PlanRun,
            session_id: None,
            user_id: None,
            tags: vec![format!("plan:{identifier}")],
        }
    }

    pub fn conversation(command: &str, thread_id: Option<String>) -> Self {
        Self {
            name: command.to_string(),
            kind: RunKind::Conversation,
            session_id: thread_id,
            user_id: None,
            tags: vec![format!("command:{command}")],
        }
    }

    pub fn draft() -> Self {
        Self {
            name: "plan draft".to_string(),
            kind: RunKind::Draft,
            session_id: None,
            user_id: None,
            tags: vec!["command:plan-draft".to_string()],
        }
    }

    pub fn user(mut self, name: Option<&str>) -> Self {
        self.user_id = name.map(str::to_string);
        self
    }
}

/// `sink`, teed with an exporting sink for `run` when telemetry is on;
/// `sink` unchanged otherwise.
pub fn attach(sink: Arc<dyn EventSink>, run: RunInfo) -> Arc<dyn EventSink> {
    let Some(telemetry) = TELEMETRY.get() else {
        return sink;
    };
    let exporting = OtlpSink::new(
        telemetry.tracer.clone(),
        run,
        telemetry.capture_content,
        telemetry.common.clone(),
    );
    Arc::new(TeeSink::new(vec![sink, Arc::new(exporting)]))
}

/// An [`EventSink`] that turns one run's events into a span tree.
pub struct OtlpSink {
    tracer: SdkTracer,
    run: RunInfo,
    capture: bool,
    common: Vec<KeyValue>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    root: Option<Context>,
    /// Open step spans in start order, keyed by plan-qualified path.
    steps: Vec<(String, Context)>,
    /// Tool calls started and not yet finished, with the step they belong to.
    tools: Vec<PendingTool>,
}

struct PendingTool {
    name: String,
    args: Option<Value>,
    step: Option<String>,
    started: SystemTime,
}

impl OtlpSink {
    pub fn new(tracer: SdkTracer, run: RunInfo, capture: bool, mut common: Vec<KeyValue>) -> Self {
        if let Some(session) = &run.session_id {
            common.push(KeyValue::new("session.id", session.clone()));
        }
        if let Some(user) = &run.user_id {
            common.push(KeyValue::new("user.id", user.clone()));
        }
        if !run.tags.is_empty() {
            common.push(KeyValue::new(
                "langfuse.trace.tags",
                OtelValue::Array(opentelemetry::Array::String(
                    run.tags.iter().cloned().map(Into::into).collect(),
                )),
            ));
        }
        Self {
            tracer,
            run,
            capture,
            common,
            state: Mutex::new(State::default()),
        }
    }

    fn root(&self, state: &mut State) -> Context {
        if let Some(root) = &state.root {
            return root.clone();
        }
        let kind = match self.run.kind {
            RunKind::PlanRun => "chain",
            RunKind::Conversation => "agent",
            RunKind::Draft => "chain",
        };
        let mut attributes = self.common.clone();
        attributes.push(KeyValue::new("langfuse.observation.type", kind));
        attributes.push(KeyValue::new(
            "graph.run.kind",
            format!("{:?}", self.run.kind).to_lowercase(),
        ));
        let span = self.tracer.build_with_context(
            SpanBuilder::from_name(self.run.name.clone())
                .with_start_time(SystemTime::now())
                .with_attributes(attributes),
            &Context::new(),
        );
        let root = Context::new().with_span(span);
        state.root = Some(root.clone());
        root
    }

    /// The span a new child belongs under: the longest open step whose
    /// path is a prefix of `key` (a body step under its map/decide step),
    /// else the most recently opened step still running (a nested plan's
    /// step under the call that entered it), else the root.
    fn parent_for(&self, state: &mut State, key: Option<&str>) -> Context {
        if let Some(key) = key {
            let mut prefix = key;
            while let Some(cut) = prefix.rfind('/') {
                prefix = &prefix[..cut];
                if let Some((_, cx)) = state.steps.iter().rev().find(|(k, _)| k == prefix) {
                    return cx.clone();
                }
            }
        }
        if let Some((_, cx)) = state.steps.last() {
            return cx.clone();
        }
        self.root(state)
    }

    fn step_context(&self, state: &mut State, key: &str) -> Context {
        match state.steps.iter().rev().find(|(k, _)| k == key) {
            Some((_, cx)) => cx.clone(),
            None => self.parent_for(state, Some(key)),
        }
    }

    fn json_attribute(&self, key: &'static str, value: &Value) -> Option<KeyValue> {
        if !self.capture {
            return None;
        }
        let mut text = serde_json::to_string(value).unwrap_or_default();
        if text.len() > MAX_ATTRIBUTE_BYTES {
            let mut cut = MAX_ATTRIBUTE_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("…[truncated]");
        }
        Some(KeyValue::new(key, text))
    }

    fn root_event(&self, name: &'static str, attributes: Vec<KeyValue>) {
        let mut state = self.state.lock().unwrap();
        let root = self.root(&mut state);
        root.span().add_event(name, attributes);
    }

    fn end_all(&self, state: &mut State) {
        let now = SystemTime::now();
        for (_, cx) in state.steps.drain(..) {
            cx.span().end_with_timestamp(now);
        }
        state.tools.clear();
        if let Some(root) = state.root.take() {
            root.span().end_with_timestamp(now);
        }
    }
}

fn qualify(call_stack: &[String], path: &str) -> String {
    if call_stack.is_empty() {
        path.to_string()
    } else {
        format!("{}/{path}", call_stack.join("/"))
    }
}

fn is_control(tool: &str) -> bool {
    matches!(
        tool,
        "exit" | "decide" | "filter" | "map" | "reduce" | "ask" | "plan_and_execute"
    ) || tool.starts_with(graph_core::toolbox::PLAN_TOOL_PREFIX)
}

fn observation_type(tool: &str) -> &'static str {
    if tool == "agent" {
        "agent"
    } else if is_control(tool) {
        "chain"
    } else {
        "tool"
    }
}

impl EventSink for OtlpSink {
    fn step_started(&self, call_stack: &[String], path: &str, tool: &str, input: &Value) {
        let key = qualify(call_stack, path);
        let mut state = self.state.lock().unwrap();
        let parent = self.parent_for(&mut state, Some(&key));
        let mut attributes = self.common.clone();
        attributes.extend([
            KeyValue::new("langfuse.observation.type", observation_type(tool)),
            KeyValue::new("graph.step.path", key.clone()),
            KeyValue::new("graph.step.tool", tool.to_string()),
            KeyValue::new("graph.plan.stack", call_stack.join("/")),
        ]);
        attributes.extend(self.json_attribute("langfuse.observation.input", input));
        let span = self.tracer.build_with_context(
            SpanBuilder::from_name(format!("{path} {tool}"))
                .with_start_time(SystemTime::now())
                .with_attributes(attributes),
            &parent,
        );
        state.steps.push((key, parent.with_span(span)));
    }

    fn step_finished(
        &self,
        call_stack: &[String],
        path: &str,
        _tool: &str,
        result: &Value,
        is_error: bool,
        _elapsed: Duration,
    ) {
        let key = qualify(call_stack, path);
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.steps.iter().rposition(|(k, _)| *k == key) else {
            return;
        };
        let (_, cx) = state.steps.remove(index);
        let span = cx.span();
        if let Some(output) = self.json_attribute("langfuse.observation.output", result) {
            span.set_attribute(output);
        }
        if is_error {
            let message = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("step failed")
                .to_string();
            span.set_attribute(KeyValue::new("langfuse.observation.level", "ERROR"));
            span.set_status(Status::error(message));
        }
        span.end_with_timestamp(SystemTime::now());
    }

    fn tool_started(&self, name: &str, args: &Value) {
        let mut state = self.state.lock().unwrap();
        let step = state.steps.last().map(|(k, _)| k.clone());
        state.tools.push(PendingTool {
            name: name.to_string(),
            args: self.capture.then(|| args.clone()),
            step,
            started: SystemTime::now(),
        });
    }

    fn tool_finished(&self, name: &str, elapsed: Duration, is_error: bool) {
        let now = SystemTime::now();
        let mut state = self.state.lock().unwrap();
        let pending = state
            .tools
            .iter()
            .rposition(|t| t.name == name)
            .map(|index| state.tools.remove(index));
        let (args, step, started) = match pending {
            Some(tool) => (tool.args, tool.step, tool.started),
            None => (None, None, now.checked_sub(elapsed).unwrap_or(now)),
        };
        let parent = match &step {
            Some(key) => self.step_context(&mut state, key),
            None => self.parent_for(&mut state, None),
        };
        let mut attributes = self.common.clone();
        attributes.extend([
            KeyValue::new("langfuse.observation.type", observation_type(name)),
            KeyValue::new("graph.tool.name", name.to_string()),
        ]);
        if let Some(args) = &args {
            attributes.extend(self.json_attribute("langfuse.observation.input", args));
        }
        let mut span = self.tracer.build_with_context(
            SpanBuilder::from_name(name.to_string())
                .with_start_time(started)
                .with_attributes(attributes),
            &parent,
        );
        if is_error {
            span.set_attribute(KeyValue::new("langfuse.observation.level", "ERROR"));
            span.set_status(Status::error("tool returned an error"));
        }
        span.end_with_timestamp(now);
    }

    fn llm_call(&self, call: &LlmCallEvent) {
        let now = SystemTime::now();
        let started = now.checked_sub(call.elapsed).unwrap_or(now);
        let mut state = self.state.lock().unwrap();
        let parent = self.step_context(&mut state, &call.site);
        let usage = &call.usage;
        let usage_details = serde_json::json!({
            "input": usage.input_tokens,
            "output": usage.output_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        });
        let mut attributes = self.common.clone();
        attributes.extend([
            KeyValue::new("langfuse.observation.type", "generation"),
            KeyValue::new("gen_ai.operation.name", "chat"),
            KeyValue::new("gen_ai.system", call.provider.clone()),
            KeyValue::new("gen_ai.request.model", call.model.clone()),
            KeyValue::new("gen_ai.response.model", call.model.clone()),
            KeyValue::new("gen_ai.usage.input_tokens", usage.input_tokens as i64),
            KeyValue::new("gen_ai.usage.output_tokens", usage.output_tokens as i64),
            KeyValue::new(
                "gen_ai.usage.cache_read_input_tokens",
                usage.cache_read_input_tokens as i64,
            ),
            KeyValue::new(
                "gen_ai.usage.cache_creation_input_tokens",
                usage.cache_creation_input_tokens as i64,
            ),
            KeyValue::new(
                "langfuse.observation.usage_details",
                usage_details.to_string(),
            ),
            KeyValue::new("graph.site", call.site.clone()),
            KeyValue::new("graph.role", call.role.clone()),
        ]);
        if let Some(cost) = call.cost_usd {
            attributes.push(KeyValue::new("graph.cost_usd", cost));
            attributes.push(KeyValue::new(
                "langfuse.observation.cost_details",
                serde_json::json!({ "total": cost }).to_string(),
            ));
        }
        if let Some(input) = &call.input {
            attributes.extend(self.json_attribute("langfuse.observation.input", input));
            attributes.extend(self.json_attribute("gen_ai.input.messages", input));
        }
        if let Some(output) = &call.output {
            attributes.extend(self.json_attribute("langfuse.observation.output", output));
            attributes.extend(self.json_attribute("gen_ai.output.messages", output));
        }
        let mut span = self.tracer.build_with_context(
            SpanBuilder::from_name(call.role.clone())
                .with_kind(SpanKind::Client)
                .with_start_time(started)
                .with_attributes(attributes),
            &parent,
        );
        span.end_with_timestamp(now);
    }

    fn usage_summary(&self, report: &UsageReport) {
        let mut state = self.state.lock().unwrap();
        let root = self.root(&mut state);
        let span = root.span();
        span.set_attributes([
            KeyValue::new("graph.usage.calls", report.calls as i64),
            KeyValue::new(
                "graph.usage.input_tokens",
                report.total.total_input_tokens() as i64,
            ),
            KeyValue::new(
                "graph.usage.output_tokens",
                report.total.output_tokens as i64,
            ),
        ]);
        if let Some(cost) = report.cost_usd {
            span.set_attribute(KeyValue::new("graph.usage.cost_usd", cost));
        }
        self.end_all(&mut state);
    }

    fn iteration(&self, n: u32) {
        self.root_event("agent_round", vec![KeyValue::new("n", i64::from(n))]);
    }

    fn replanning(&self, attempt: u32) {
        self.root_event(
            "replanning",
            vec![KeyValue::new("attempt", i64::from(attempt))],
        );
    }

    fn planning(&self) {
        self.root_event("planning", Vec::new());
    }

    fn synthesizing(&self) {
        self.root_event("synthesizing", Vec::new());
    }

    fn draft_outline(&self, items: &Value) {
        let count = items.as_array().map_or(0, Vec::len);
        self.root_event("draft_outline", vec![KeyValue::new("stages", count as i64)]);
    }

    fn draft_step_started(&self, index: usize, summary: &str) {
        self.root_event(
            "draft_step_started",
            vec![
                KeyValue::new("index", index as i64),
                KeyValue::new("summary", summary.to_string()),
            ],
        );
    }

    fn draft_step_finished(&self, index: usize, _step: &Value, problems: &[String], attempt: u32) {
        self.root_event(
            "draft_step_finished",
            vec![
                KeyValue::new("index", index as i64),
                KeyValue::new("attempt", i64::from(attempt)),
                KeyValue::new("accepted", problems.is_empty()),
            ],
        );
    }
}

impl Drop for OtlpSink {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        self.end_all(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_llm::types::Usage;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
    use serde_json::json;

    fn harness(run: RunInfo, capture: bool) -> (OtlpSink, InMemorySpanExporter, SdkTracerProvider) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");
        let sink = OtlpSink::new(
            tracer,
            run,
            capture,
            vec![KeyValue::new("deployment.environment", "test")],
        );
        (sink, exporter, provider)
    }

    fn call(site: &str, input: Option<Value>) -> LlmCallEvent {
        LlmCallEvent {
            site: site.into(),
            role: "solver".into(),
            provider: "anthropic".into(),
            model: "claude-haiku-4-5".into(),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 3,
                ..Default::default()
            },
            elapsed: Duration::from_millis(20),
            cost_usd: Some(0.5),
            input,
            output: None,
        }
    }

    fn by_name<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no span named {name:?} in {:?}", names(spans)))
    }

    fn names(spans: &[SpanData]) -> Vec<String> {
        spans.iter().map(|s| s.name.to_string()).collect()
    }

    fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a OtelValue> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    #[test]
    fn a_plan_run_becomes_a_tree_of_steps_tools_and_generations() {
        let (sink, exporter, provider) = harness(RunInfo::plan_run("sprint_analysis"), false);
        let none: Vec<String> = Vec::new();

        sink.step_started(&none, "E0", "user__git_log", &json!({"n": 3}));
        sink.tool_started("user__git_log", &json!({"n": 3}));
        sink.tool_finished("user__git_log", Duration::from_millis(5), false);
        sink.step_finished(
            &none,
            "E0",
            "user__git_log",
            &json!(["c1"]),
            false,
            Duration::ZERO,
        );

        sink.step_started(&none, "E1", "map", &json!({}));
        sink.step_started(&none, "E1/do.0/E5", "builtin__infer", &json!({}));
        sink.llm_call(&call("E1/do.0/E5", None));
        sink.step_finished(
            &none,
            "E1/do.0/E5",
            "builtin__infer",
            &json!("x"),
            false,
            Duration::ZERO,
        );
        sink.step_finished(&none, "E1", "map", &json!([]), false, Duration::ZERO);

        sink.step_started(&none, "E2", "plan__inner", &json!({}));
        let inner = vec!["inner".to_string()];
        sink.step_started(&inner, "E0", "user__git_log", &json!({}));
        sink.step_finished(
            &inner,
            "E0",
            "user__git_log",
            &json!({"error": "boom"}),
            true,
            Duration::ZERO,
        );
        sink.step_finished(
            &none,
            "E2",
            "plan__inner",
            &json!({}),
            false,
            Duration::ZERO,
        );

        sink.llm_call(&call("solver", None));
        sink.usage_summary(&UsageReport {
            calls: 2,
            cost_usd: Some(1.0),
            ..Default::default()
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let root = by_name(&spans, "sprint_analysis");
        assert_eq!(root.parent_span_id, opentelemetry::trace::SpanId::INVALID);
        assert!(spans
            .iter()
            .all(|s| s.span_context.trace_id() == root.span_context.trace_id()));
        assert_eq!(
            attribute(root, "graph.usage.cost_usd"),
            Some(&OtelValue::F64(1.0))
        );

        let e0 = by_name(&spans, "E0 user__git_log");
        assert_eq!(e0.parent_span_id, root.span_context.span_id());
        let tool = by_name(&spans, "user__git_log");
        assert_eq!(tool.parent_span_id, e0.span_context.span_id());
        assert!(attribute(tool, "langfuse.observation.input").is_none());

        let e1 = by_name(&spans, "E1 map");
        let body = by_name(&spans, "E1/do.0/E5 builtin__infer");
        assert_eq!(body.parent_span_id, e1.span_context.span_id());
        let generation = spans
            .iter()
            .find(|s| attribute(s, "graph.site") == Some(&OtelValue::from("E1/do.0/E5")))
            .unwrap();
        assert_eq!(generation.parent_span_id, body.span_context.span_id());
        assert_eq!(
            attribute(generation, "langfuse.observation.type"),
            Some(&OtelValue::from("generation"))
        );
        assert_eq!(
            attribute(generation, "gen_ai.usage.input_tokens"),
            Some(&OtelValue::I64(10))
        );
        assert_eq!(
            attribute(generation, "langfuse.observation.cost_details"),
            Some(&OtelValue::from(r#"{"total":0.5}"#))
        );

        let e2 = by_name(&spans, "E2 plan__inner");
        let inner_step = by_name(&spans, "E0 user__git_log");
        let inner_step = spans
            .iter()
            .filter(|s| s.name == inner_step.name)
            .find(|s| attribute(s, "graph.plan.stack") == Some(&OtelValue::from("inner")))
            .unwrap();
        assert_eq!(inner_step.parent_span_id, e2.span_context.span_id());
        assert!(matches!(inner_step.status, Status::Error { .. }));

        let solver = spans
            .iter()
            .find(|s| attribute(s, "graph.site") == Some(&OtelValue::from("solver")))
            .unwrap();
        assert_eq!(solver.parent_span_id, root.span_context.span_id());
        assert!(spans
            .iter()
            .all(|s| { attribute(s, "deployment.environment") == Some(&OtelValue::from("test")) }));
    }

    #[test]
    fn content_lands_on_spans_only_when_captured() {
        let none: Vec<String> = Vec::new();
        for capture in [false, true] {
            let run = RunInfo::conversation("ask", Some("thread-1".into())).user(Some("tyler"));
            let (sink, exporter, provider) = harness(run, capture);
            sink.tool_started("builtin__reshape", &json!({"secret": 1}));
            sink.tool_finished("builtin__reshape", Duration::from_millis(1), false);
            sink.step_started(&none, "E0", "builtin__reshape", &json!({"in": 1}));
            sink.step_finished(
                &none,
                "E0",
                "builtin__reshape",
                &json!({"out": 2}),
                false,
                Duration::ZERO,
            );
            sink.llm_call(&call("chat", Some(json!({"system": "s"}))));
            drop(sink);
            provider.force_flush().unwrap();

            let spans = exporter.get_finished_spans().unwrap();
            let root = by_name(&spans, "ask");
            assert_eq!(
                attribute(root, "session.id"),
                Some(&OtelValue::from("thread-1"))
            );
            assert_eq!(attribute(root, "user.id"), Some(&OtelValue::from("tyler")));
            let tool = by_name(&spans, "builtin__reshape");
            let step = by_name(&spans, "E0 builtin__reshape");
            let generation = by_name(&spans, "solver");
            let has = |span: &SpanData, key: &str| attribute(span, key).is_some();
            assert_eq!(has(tool, "langfuse.observation.input"), capture);
            assert_eq!(has(step, "langfuse.observation.input"), capture);
            assert_eq!(has(step, "langfuse.observation.output"), capture);
            assert_eq!(has(generation, "langfuse.observation.input"), capture);
            assert_eq!(has(generation, "gen_ai.input.messages"), capture);
        }
    }

    #[test]
    fn dropping_the_sink_ends_whatever_is_still_open() {
        let (sink, exporter, provider) = harness(RunInfo::draft(), false);
        sink.planning();
        sink.step_started(&[], "E0", "user__x", &json!({}));
        drop(sink);
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(names(&spans).len(), 2, "{:?}", names(&spans));
        let root = by_name(&spans, "plan draft");
        assert_eq!(root.events.len(), 1);
        assert_eq!(root.events[0].name, "planning");
    }
}
