//! Modal editor state: the JSON editors (inject a result at a pause,
//! collect run inputs) and the quit confirm. Each editor explains itself:
//! a header says what the JSON becomes, where the prefill came from, and
//! which fields downstream templates will read. Key handling lives in
//! `app`; rendering in `ui`.

use super::app::{GateKind, GatePrompt};
use serde_json::Value;
use tui_textarea::TextArea;

pub enum EditorContext {
    /// Deciding a paused call: the JSON typed here becomes the step's
    /// result (before-call skip) or replaces the failed result (on-error).
    /// Carries the whole parked prompt so Esc restores the pause.
    InjectResult {
        prompt: GatePrompt,
    },
    /// Collecting the plan's input object before a run.
    RunInput {
        gated: bool,
    },
    ConfirmQuit,
}

pub struct EditorState {
    pub title: String,
    /// Guidance lines rendered above the textarea.
    pub header: Vec<String>,
    pub context: EditorContext,
    pub textarea: TextArea<'static>,
    pub error: Option<String>,
}

impl EditorState {
    pub fn inject_result(
        prompt: GatePrompt,
        prefill: Value,
        provenance: &str,
        downstream: Vec<String>,
    ) -> Self {
        let (title, what) = match &prompt.kind {
            GateKind::BeforeCall => (
                format!("inject result for {} ({})", prompt.path, prompt.tool),
                format!(
                    "This JSON becomes {}'s result — the tool will NOT be called.",
                    prompt.path
                ),
            ),
            GateKind::OnError { .. } => (
                format!(
                    "replace failed result for {} ({})",
                    prompt.path, prompt.tool
                ),
                format!(
                    "The call failed. This JSON replaces {}'s error and the run continues.",
                    prompt.path
                ),
            ),
        };
        let mut header = vec![what, format!("prefill: {provenance}")];
        if !downstream.is_empty() {
            header.push(format!(
                "downstream templates read: {}",
                downstream.join(", ")
            ));
        }
        Self {
            title,
            header,
            textarea: json_textarea(&prefill),
            context: EditorContext::InjectResult { prompt },
            error: None,
        }
    }

    pub fn run_input(gated: bool, schema: &Value, defaults: &Value) -> Self {
        // Seed the editor with schema defaults plus nulls for every
        // required key still missing, so the user edits instead of typing
        // an object from scratch.
        let mut seed = defaults.clone();
        let mut required_keys: Vec<&str> = Vec::new();
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            required_keys = required.iter().filter_map(Value::as_str).collect();
            if let Some(object) = seed.as_object_mut() {
                for key in &required_keys {
                    object.entry(key.to_string()).or_insert(Value::Null);
                }
            }
        }
        let mut header = vec!["These values become {{input.*}} for the run.".to_string()];
        if !required_keys.is_empty() {
            header.push(format!("required: {}", required_keys.join(", ")));
        }
        Self {
            title: "plan inputs (JSON)".to_string(),
            header,
            textarea: json_textarea(&seed),
            context: EditorContext::RunInput { gated },
            error: None,
        }
    }

    pub fn confirm_quit(reason: &str) -> Self {
        Self {
            title: reason.to_string(),
            header: Vec::new(),
            textarea: TextArea::default(),
            context: EditorContext::ConfirmQuit,
            error: None,
        }
    }
}

/// A typed placeholder value synthesized from a JSON Schema, for prefilling
/// the inject editor when a tool declares a schema but no example.
pub fn schema_skeleton(schema: &Value) -> Value {
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let mut object = serde_json::Map::new();
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, property) in properties {
                    object.insert(key.clone(), schema_skeleton(property));
                }
            }
            Value::Object(object)
        }
        Some("array") => match schema.get("items") {
            Some(items) => Value::Array(vec![schema_skeleton(items)]),
            None => Value::Array(Vec::new()),
        },
        Some("string") => Value::String(String::new()),
        Some("number") | Some("integer") => Value::Number(0.into()),
        Some("boolean") => Value::Bool(false),
        _ => Value::Null,
    }
}

fn json_textarea(value: &Value) -> TextArea<'static> {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
    TextArea::from(text.lines().map(str::to_string).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_skeleton_synthesizes_typed_placeholders() {
        let schema = json!({
            "type": "object",
            "properties": {
                "values": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "count": {"type": "integer"},
                            "done": {"type": "boolean"}
                        }
                    }
                },
                "note": {"type": "string"}
            }
        });
        assert_eq!(
            schema_skeleton(&schema),
            json!({
                "values": [{"id": "", "count": 0, "done": false}],
                "note": ""
            })
        );
        assert_eq!(schema_skeleton(&json!({})), Value::Null);
    }
}
