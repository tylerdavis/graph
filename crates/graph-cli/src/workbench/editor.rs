//! Modal editor state: the JSON editors (inject a skipped tool's result,
//! collect run inputs) and the quit confirm. Key handling lives in `app`.

use graph_core::pipeline::GateDecision;
use serde_json::Value;
use tokio::sync::oneshot;
use tui_textarea::TextArea;

pub enum EditorContext {
    /// Skipping a gated tool call: the JSON typed here becomes the step's
    /// result. Carries the parked gate reply.
    InjectResult {
        path: String,
        tool: String,
        input: Value,
        reply: Option<oneshot::Sender<GateDecision>>,
    },
    /// Collecting the plan's input object before a run.
    RunInput {
        gated: bool,
    },
    ConfirmQuit,
}

pub struct EditorState {
    pub title: String,
    pub context: EditorContext,
    pub textarea: TextArea<'static>,
    pub error: Option<String>,
}

impl EditorState {
    pub fn inject_result(
        path: String,
        tool: String,
        input: Value,
        reply: Option<oneshot::Sender<GateDecision>>,
        prefill: Option<Value>,
    ) -> Self {
        let seed = prefill.unwrap_or(Value::Null);
        Self {
            title: format!("inject result for {path} ({tool})"),
            textarea: json_textarea(&seed),
            context: EditorContext::InjectResult {
                path,
                tool,
                input,
                reply,
            },
            error: None,
        }
    }

    pub fn run_input(gated: bool, schema: &Value, defaults: &Value) -> Self {
        // Seed the editor with schema defaults plus nulls for every
        // required key still missing, so the user edits instead of typing
        // an object from scratch.
        let mut seed = defaults.clone();
        if let (Some(required), Some(object)) = (
            schema.get("required").and_then(Value::as_array),
            seed.as_object_mut(),
        ) {
            for key in required.iter().filter_map(Value::as_str) {
                object.entry(key.to_string()).or_insert(Value::Null);
            }
        }
        Self {
            title: "plan inputs (JSON)".to_string(),
            textarea: json_textarea(&seed),
            context: EditorContext::RunInput { gated },
            error: None,
        }
    }

    pub fn confirm_quit(reason: &str) -> Self {
        Self {
            title: reason.to_string(),
            textarea: TextArea::default(),
            context: EditorContext::ConfirmQuit,
            error: None,
        }
    }
}

fn json_textarea(value: &Value) -> TextArea<'static> {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
    TextArea::from(text.lines().map(str::to_string).collect::<Vec<_>>())
}
