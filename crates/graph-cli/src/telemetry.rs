use graph_config::TelemetryConfig;
use graph_core::usage::{LlmCallEvent, UsageReport};
use graph_core::{EventSink, RunStart, TeeSink};
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
const EXPORT_BATCH_SPANS: usize = 16;
const EXPORT_QUEUE_SPANS: usize = 8192;
const EXPORT_DELAY: Duration = Duration::from_secs(2);
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(120);

static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();

struct Telemetry {
    tracer_provider: SdkTracerProvider,
    logger_provider: Option<SdkLoggerProvider>,
    tracer: SdkTracer,
    capture_content: bool,
    common: Vec<KeyValue>,
}

#[derive(Default)]
pub struct Init {
    pub active: bool,
    pub warnings: Vec<String>,
}

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

    let http_client = reqwest::Client::builder()
        .http1_only()
        .pool_max_idle_per_host(0)
        .timeout(timeout)
        .build()?;

    let traces_url = config
        .signal_url("v1/traces")
        .ok_or_else(|| anyhow::anyhow!("no telemetry endpoint"))?;
    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(http_client.clone())
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
        .with_batch_config(
            opentelemetry_sdk::trace::BatchConfigBuilder::default()
                .with_max_export_batch_size(EXPORT_BATCH_SPANS)
                .with_max_queue_size(EXPORT_QUEUE_SPANS)
                .with_scheduled_delay(EXPORT_DELAY)
                .with_max_export_timeout(timeout)
                .build(),
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
            .with_http_client(http_client)
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
            .with_batch_config(
                opentelemetry_sdk::logs::BatchConfigBuilder::default()
                    .with_scheduled_delay(EXPORT_DELAY)
                    .with_max_export_timeout(timeout)
                    .build(),
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

pub fn captures_content() -> bool {
    TELEMETRY.get().is_some_and(|t| t.capture_content)
}

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

pub async fn shutdown() {
    let Some(telemetry) = TELEMETRY.get() else {
        return;
    };
    let _ = tokio::task::spawn_blocking(|| {
        if let Err(error) = telemetry
            .tracer_provider
            .shutdown_with_timeout(SHUTDOWN_FLUSH)
        {
            tracing::warn!(%error, "telemetry: trace export did not finish before exit");
        }
        if let Some(logs) = &telemetry.logger_provider {
            if let Err(error) = logs.shutdown_with_timeout(SHUTDOWN_FLUSH) {
                tracing::warn!(%error, "telemetry: log export did not finish before exit");
            }
        }
    })
    .await;
}

#[derive(Debug, Clone)]
pub struct RunInfo {
    pub name: String,
    pub kind: RunKind,
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub tags: Vec<String>,
    pub input: Option<Value>,
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
            input: None,
        }
    }

    pub fn conversation(command: &str, thread_id: Option<String>) -> Self {
        Self {
            name: command.to_string(),
            kind: RunKind::Conversation,
            session_id: thread_id,
            user_id: None,
            tags: vec![format!("command:{command}")],
            input: None,
        }
    }

    pub fn draft() -> Self {
        Self {
            name: "plan draft".to_string(),
            kind: RunKind::Draft,
            session_id: None,
            user_id: None,
            tags: vec!["command:plan-draft".to_string()],
            input: None,
        }
    }

    pub fn user(mut self, name: Option<&str>) -> Self {
        self.user_id = name.map(str::to_string);
        self
    }

    pub fn input(mut self, input: Value) -> Self {
        self.input = Some(input);
        self
    }

    pub fn session(mut self, id: Option<String>) -> Self {
        self.session_id = id;
        self
    }
}

pub fn plan_output(outcome: &graph_core::pipeline::PipelineOutcome) -> Value {
    if let Some(structured) = &outcome.structured {
        return structured.clone();
    }
    if !outcome.answer.is_empty() {
        return Value::String(outcome.answer.clone());
    }
    match &outcome.exit {
        Some(exit) => serde_json::json!({ "exit": exit.status, "message": exit.message }),
        None => Value::Null,
    }
}

pub fn report_plan_result(
    events: &dyn EventSink,
    result: &Result<graph_core::pipeline::PipelineOutcome, graph_core::pipeline::PipelineError>,
) {
    match result {
        Ok(outcome) => {
            let asserted = matches!(
                &outcome.exit,
                Some(exit) if exit.status == graph_core::pipeline::ExitStatus::Error
            );
            events.run_finished(&plan_output(outcome), asserted);
        }
        Err(error) => {
            events.run_finished(&serde_json::json!({ "error": error.to_string() }), true);
        }
    }
}

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
    steps: Vec<(String, Context)>,
    run: Option<RunStart>,
}

fn clip_strings(value: &mut Value, cap: usize) {
    match value {
        Value::String(text) if text.len() > cap => {
            let mut cut = cap;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let dropped = text.len() - cut;
            text.truncate(cut);
            text.push_str(&format!("…[{dropped} bytes truncated]"));
        }
        Value::Array(items) => items.iter_mut().for_each(|item| clip_strings(item, cap)),
        Value::Object(map) => map.values_mut().for_each(|item| clip_strings(item, cap)),
        _ => {}
    }
}

fn clipped_json(value: &Value, budget: usize) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    if text.len() <= budget {
        return text;
    }
    let mut cap = budget / 2;
    while cap >= 64 {
        let mut clipped = value.clone();
        clip_strings(&mut clipped, cap);
        let text = serde_json::to_string(&clipped).unwrap_or_default();
        if text.len() <= budget {
            return text;
        }
        cap /= 2;
    }
    let mut preview = text.clone();
    let mut cut = budget / 2;
    while !preview.is_char_boundary(cut) {
        cut -= 1;
    }
    preview.truncate(cut);
    serde_json::json!({
        "truncated": true,
        "bytes": text.len(),
        "preview": preview,
    })
    .to_string()
}

impl OtlpSink {
    pub fn new(tracer: SdkTracer, run: RunInfo, capture: bool, mut common: Vec<KeyValue>) -> Self {
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

    fn attrs(&self, state: &State) -> Vec<KeyValue> {
        let mut attributes = self.common.clone();
        let session = state
            .run
            .as_ref()
            .and_then(|run| run.session_id.as_ref())
            .or(self.run.session_id.as_ref());
        if let Some(session) = session {
            attributes.push(KeyValue::new("session.id", session.clone()));
        }
        attributes
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
        let name = state
            .run
            .as_ref()
            .map_or(self.run.name.as_str(), |run| run.name.as_str())
            .to_string();
        let input = state
            .run
            .as_ref()
            .and_then(|run| run.input.as_ref())
            .or(self.run.input.as_ref())
            .cloned();
        let mut attributes = self.attrs(state);
        attributes.push(KeyValue::new("langfuse.observation.type", kind));
        attributes.push(KeyValue::new(
            "graph.run.kind",
            format!("{:?}", self.run.kind).to_lowercase(),
        ));
        if let Some(input) = &input {
            attributes.extend(self.json_attribute("langfuse.observation.input", input));
        }
        let span = self.tracer.build_with_context(
            SpanBuilder::from_name(name)
                .with_start_time(SystemTime::now())
                .with_attributes(attributes),
            &Context::new(),
        );
        let root = Context::new().with_span(span);
        state.root = Some(root.clone());
        root
    }

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
        Some(KeyValue::new(key, clipped_json(value, MAX_ATTRIBUTE_BYTES)))
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

fn is_bare_control(tool: &str) -> bool {
    matches!(tool, "exit" | "decide" | "filter" | "map" | "reduce")
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
        let mut attributes = self.attrs(&state);
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
        if is_bare_control(name) {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if !state.steps.is_empty() {
            return;
        }
        let parent = self.root(&mut state);
        let mut attributes = self.attrs(&state);
        attributes.extend([
            KeyValue::new("langfuse.observation.type", observation_type(name)),
            KeyValue::new("graph.tool.name", name.to_string()),
        ]);
        attributes.extend(self.json_attribute("langfuse.observation.input", args));
        let span = self.tracer.build_with_context(
            SpanBuilder::from_name(name.to_string())
                .with_start_time(SystemTime::now())
                .with_attributes(attributes),
            &parent,
        );
        state
            .steps
            .push((format!("tool:{name}"), parent.with_span(span)));
    }

    fn tool_finished(&self, name: &str, _elapsed: Duration, is_error: bool) {
        if is_bare_control(name) {
            return;
        }
        let key = format!("tool:{name}");
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.steps.iter().rposition(|(k, _)| *k == key) else {
            return;
        };
        let (_, cx) = state.steps.remove(index);
        let span = cx.span();
        if is_error {
            span.set_attribute(KeyValue::new("langfuse.observation.level", "ERROR"));
            span.set_status(Status::error("tool returned an error"));
        }
        span.end_with_timestamp(SystemTime::now());
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
        let mut attributes = self.attrs(&state);
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
        }
        if let Some(output) = &call.output {
            attributes.extend(self.json_attribute("langfuse.observation.output", output));
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

    fn run_started(&self, run: &RunStart) {
        let mut state = self.state.lock().unwrap();
        self.end_all(&mut state);
        state.run = Some(run.clone());
        self.root(&mut state);
    }

    fn run_finished(&self, output: &Value, is_error: bool) {
        let mut state = self.state.lock().unwrap();
        let root = self.root(&mut state);
        let span = root.span();
        if let Some(output) = self.json_attribute("langfuse.observation.output", output) {
            span.set_attribute(output);
        }
        if is_error {
            let message = output
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("run failed")
                .to_string();
            span.set_attribute(KeyValue::new("langfuse.observation.level", "ERROR"));
            span.set_status(Status::error(message));
        }
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
        sink.tool_started("map", &json!({}));
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
        assert!(
            !names(&spans).iter().any(|n| n == "user__git_log"),
            "a tool call inside a step is the step span: {:?}",
            names(&spans)
        );

        let e1 = by_name(&spans, "E1 map");
        assert!(
            !names(&spans).iter().any(|n| n == "map"),
            "a control step's own tool pair is not a span: {:?}",
            names(&spans)
        );
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
            assert!(!has(generation, "gen_ai.input.messages"));
        }

        let (sink, exporter, provider) = harness(RunInfo::plan_run("p"), true);
        sink.step_started(&[], "E0", "user__x", &json!({"big": 1}));
        sink.tool_started("user__x", &json!({"big": 1}));
        sink.tool_finished("user__x", Duration::from_millis(1), false);
        sink.step_finished(&[], "E0", "user__x", &json!(2), false, Duration::ZERO);
        drop(sink);
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert!(attribute(by_name(&spans, "E0 user__x"), "langfuse.observation.input").is_some());
        assert!(!names(&spans).iter().any(|n| n == "user__x"));
    }

    #[test]
    fn a_conversation_tool_call_holds_the_plan_it_invokes() {
        let (sink, exporter, provider) = harness(RunInfo::conversation("ask", None), true);
        sink.tool_started("plan__inner", &json!({"team": "core"}));
        let inner = vec!["inner".to_string()];
        sink.step_started(&inner, "E0", "user__git_log", &json!({}));
        sink.step_finished(
            &inner,
            "E0",
            "user__git_log",
            &json!([]),
            false,
            Duration::ZERO,
        );
        sink.tool_finished("plan__inner", Duration::from_millis(3), true);
        sink.tool_started("builtin__reshape", &json!({}));
        sink.tool_finished("builtin__reshape", Duration::from_millis(1), false);
        drop(sink);
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let root = by_name(&spans, "ask");
        let call = by_name(&spans, "plan__inner");
        assert_eq!(call.parent_span_id, root.span_context.span_id());
        assert!(matches!(call.status, Status::Error { .. }));
        assert_eq!(
            attribute(call, "langfuse.observation.input"),
            Some(&OtelValue::from(r#"{"team":"core"}"#))
        );
        let step = by_name(&spans, "E0 user__git_log");
        assert_eq!(step.parent_span_id, call.span_context.span_id());
        let reshape = by_name(&spans, "builtin__reshape");
        assert_eq!(reshape.parent_span_id, root.span_context.span_id());
    }

    #[test]
    fn each_run_started_opens_a_fresh_trace_in_the_session() {
        let run = RunInfo::conversation("workbench", None).session(Some("wb-1".into()));
        let (sink, exporter, provider) = harness(run, true);
        sink.run_started(&RunStart {
            name: "sprint_analysis".into(),
            session_id: None,
            input: Some(json!({"team": "core"})),
        });
        sink.step_started(&[], "E0", "user__x", &json!({}));
        sink.step_finished(&[], "E0", "user__x", &json!(1), false, Duration::ZERO);
        sink.run_finished(&json!({"ok": true}), false);
        sink.usage_summary(&UsageReport::default());

        sink.run_started(&RunStart {
            name: "workbench".into(),
            session_id: Some("thread-9".into()),
            input: Some(json!("fix E0")),
        });
        sink.tool_started("workbench__validate_plan", &json!({}));
        sink.tool_finished("workbench__validate_plan", Duration::ZERO, false);
        sink.run_finished(&json!("done"), false);
        drop(sink);
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let first = by_name(&spans, "sprint_analysis");
        let second = by_name(&spans, "workbench");
        assert_ne!(
            first.span_context.trace_id(),
            second.span_context.trace_id(),
            "one trace per run"
        );
        assert_eq!(
            attribute(first, "langfuse.observation.input"),
            Some(&OtelValue::from(r#"{"team":"core"}"#))
        );
        assert_eq!(
            attribute(first, "langfuse.observation.output"),
            Some(&OtelValue::from(r#"{"ok":true}"#))
        );
        let step = by_name(&spans, "E0 user__x");
        assert_eq!(step.parent_span_id, first.span_context.span_id());
        assert_eq!(
            attribute(step, "session.id"),
            Some(&OtelValue::from("wb-1"))
        );
        assert_eq!(
            attribute(first, "session.id"),
            Some(&OtelValue::from("wb-1"))
        );
        let tool = by_name(&spans, "workbench__validate_plan");
        assert_eq!(tool.parent_span_id, second.span_context.span_id());
        assert_eq!(
            attribute(tool, "session.id"),
            Some(&OtelValue::from("thread-9"))
        );
        assert_eq!(
            attribute(second, "session.id"),
            Some(&OtelValue::from("thread-9"))
        );
    }

    #[test]
    fn clipped_content_stays_valid_json() {
        let big = "x".repeat(200_000);
        let value = json!({"instruction": "review", "data": big, "items": ["a", "b"]});
        let text = clipped_json(&value, MAX_ATTRIBUTE_BYTES);
        assert!(text.len() <= MAX_ATTRIBUTE_BYTES, "{}", text.len());
        let parsed: Value = serde_json::from_str(&text).expect("still JSON");
        assert_eq!(parsed["instruction"], "review");
        assert_eq!(parsed["items"], json!(["a", "b"]));
        let data = parsed["data"].as_str().unwrap();
        assert!(
            data.starts_with("xxxx") && data.contains("bytes truncated]"),
            "{data}"
        );

        let small = json!({"said": "hi"});
        assert_eq!(
            clipped_json(&small, MAX_ATTRIBUTE_BYTES),
            r#"{"said":"hi"}"#
        );

        let wide = json!((0..20_000).map(|i| i.to_string()).collect::<Vec<_>>());
        let text = clipped_json(&wide, 4_096);
        assert!(text.len() <= 4_096 + 64, "{}", text.len());
        let parsed: Value = serde_json::from_str(&text).expect("still JSON");
        assert_eq!(parsed["truncated"], true);
    }

    #[test]
    fn the_root_carries_the_run_input_and_output_when_captured() {
        for capture in [false, true] {
            let run = RunInfo::plan_run("echo").input(json!({"word": "hi"}));
            let (sink, exporter, provider) = harness(run, capture);
            sink.step_started(&[], "E1", "builtin__reshape", &json!({}));
            sink.step_finished(
                &[],
                "E1",
                "builtin__reshape",
                &json!({}),
                false,
                Duration::ZERO,
            );
            sink.run_finished(&json!({"said": "hi"}), false);
            sink.usage_summary(&UsageReport::default());
            provider.force_flush().unwrap();

            let spans = exporter.get_finished_spans().unwrap();
            let root = by_name(&spans, "echo");
            assert_eq!(
                attribute(root, "langfuse.observation.input").is_some(),
                capture
            );
            assert_eq!(
                attribute(root, "langfuse.observation.output"),
                capture
                    .then(|| OtelValue::from(r#"{"said":"hi"}"#))
                    .as_ref()
            );
            assert!(matches!(root.status, Status::Unset));
        }

        let (sink, exporter, provider) = harness(RunInfo::plan_run("broken"), false);
        sink.run_finished(&json!({"error": "E1 failed"}), true);
        drop(sink);
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let root = by_name(&spans, "broken");
        assert!(
            matches!(&root.status, Status::Error { description } if description == "E1 failed")
        );
        assert_eq!(
            attribute(root, "langfuse.observation.level"),
            Some(&OtelValue::from("ERROR"))
        );
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
