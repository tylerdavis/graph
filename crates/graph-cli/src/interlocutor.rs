//! The terminal binding for `ask` steps: put the question on stderr, read
//! the answer from stdin.
//!
//! Two contracts constrain this file:
//!
//! - **Streams.** stdout carries only the deliverable, so every character
//!   of the prompt goes to stderr. That also means the prompt is invisible
//!   when stderr is redirected — hence the availability check below.
//! - **Machine-readable events.** `GRAPH_EVENTS=jsonl` turns stderr into a
//!   parseable event stream; a hand-typed prompt in the middle of it would
//!   corrupt the trace. In that mode there is no interlocutor at all, and
//!   `ask` steps resolve by their declared `whenUnanswered`.

use async_trait::async_trait;
use graph_core::pipeline::{AskOutcome, AskRequest, Interlocutor};
use serde_json::{Map, Value};
use std::io::{BufRead, Write};

/// The terminal interlocutor as a pipeline hook, or `None` when this
/// process has no usable terminal. The one place callers should reach for.
pub fn tty() -> Option<std::sync::Arc<dyn Interlocutor>> {
    TtyInterlocutor::detect()
        .map(|tty| std::sync::Arc::new(tty) as std::sync::Arc<dyn Interlocutor>)
}

/// Answers `ask` steps from the controlling terminal.
pub struct TtyInterlocutor {
    /// One question at a time: a `map` body with concurrency above 1 can
    /// reach several `ask` steps at once, and two prompts interleaved on
    /// one terminal are unanswerable.
    lock: tokio::sync::Mutex<()>,
}

impl TtyInterlocutor {
    /// An interlocutor for this process, or `None` when the terminal
    /// cannot carry a conversation: stdin or stderr is redirected, or
    /// stderr is reserved for machine-readable events.
    ///
    /// Returning `None` is not a degraded mode — it is the honest input to
    /// each `ask` step's declared `whenUnanswered`, and it is what makes
    /// the same plan behave predictably under `graph plan run < /dev/null`
    /// and in CI.
    pub fn detect() -> Option<Self> {
        Self::available(
            std::env::var("GRAPH_EVENTS").as_deref() == Ok("jsonl"),
            std::io::IsTerminal::is_terminal(&std::io::stdin()),
            std::io::IsTerminal::is_terminal(&std::io::stderr()),
        )
        .then(|| Self {
            lock: tokio::sync::Mutex::new(()),
        })
    }

    fn available(machine_events: bool, stdin_tty: bool, stderr_tty: bool) -> bool {
        !machine_events && stdin_tty && stderr_tty
    }
}

#[async_trait]
impl Interlocutor for TtyInterlocutor {
    async fn ask(&self, request: AskRequest) -> AskOutcome {
        let _guard = self.lock.lock().await;
        // Reading a line parks the thread; a pipeline this size has real
        // work in flight (concurrent map items, streaming events), so it
        // must not park a runtime worker.
        tokio::task::spawn_blocking(move || prompt(&request))
            .await
            .unwrap_or_else(|e| AskOutcome::Unavailable(format!("prompt task failed: {e}")))
    }
}

/// One field of the answer form.
struct Field {
    name: String,
    description: Option<String>,
    kind: Kind,
    required: bool,
}

enum Kind {
    Text,
    Integer,
    Number,
    Boolean,
    Choice(Vec<String>),
}

fn fields(schema: &Value) -> Vec<Field> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    properties
        .iter()
        .map(|(name, property)| {
            let kind = match property.get("enum").and_then(Value::as_array) {
                Some(options) => Kind::Choice(
                    options
                        .iter()
                        .map(|o| match o {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect(),
                ),
                None => match property.get("type").and_then(Value::as_str) {
                    Some("integer") => Kind::Integer,
                    Some("number") => Kind::Number,
                    Some("boolean") => Kind::Boolean,
                    _ => Kind::Text,
                },
            };
            Field {
                name: name.clone(),
                description: property
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                kind,
                required: required.contains(&name.as_str()),
            }
        })
        .collect()
}

/// The label after the field name: what the user may type.
fn hint(kind: &Kind) -> String {
    match kind {
        Kind::Text => "text".to_string(),
        Kind::Integer => "integer".to_string(),
        Kind::Number => "number".to_string(),
        Kind::Boolean => "y/n".to_string(),
        Kind::Choice(options) => options.join(" | "),
    }
}

/// Coerce a typed line into the field's JSON type, or explain the miss.
fn coerce(kind: &Kind, text: &str) -> Result<Value, String> {
    match kind {
        Kind::Text => Ok(Value::String(text.to_string())),
        Kind::Integer => text
            .parse::<i64>()
            .map(|n| Value::Number(n.into()))
            .map_err(|_| format!("'{text}' is not an integer")),
        Kind::Number => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| format!("'{text}' is not a number")),
        Kind::Boolean => match text.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "t" | "1" => Ok(Value::Bool(true)),
            "n" | "no" | "false" | "f" | "0" => Ok(Value::Bool(false)),
            _ => Err(format!("'{text}' is not yes or no")),
        },
        Kind::Choice(options) => options
            .iter()
            .find(|option| option.eq_ignore_ascii_case(text))
            .map(|option| Value::String(option.clone()))
            .ok_or_else(|| format!("'{text}' is not one of: {}", options.join(", "))),
    }
}

/// Re-prompts allowed per field after a value that could not be coerced.
/// A field therefore gets `MAX_RETRIES + 1` prompts in total. Bounded so a
/// scripted stdin that never satisfies the schema ends the run instead of
/// spinning.
const MAX_RETRIES: usize = 3;

fn prompt(request: &AskRequest) -> AskOutcome {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines().map_while(Result::ok);
    fill_form(request, &mut lines, &mut std::io::stderr())
}

/// Walk the answer schema field by field, reading one line per prompt.
///
/// Split out from [`prompt`] so the retry budget and the coercion messages
/// are testable against a scripted reader — the loop is where the
/// documented behaviour lives, and the last drift between the two was a
/// review comment rather than a failing test.
fn fill_form(
    request: &AskRequest,
    lines: &mut impl Iterator<Item = String>,
    err: &mut impl Write,
) -> AskOutcome {
    let location = if request.call_stack.is_empty() {
        request.path.to_string()
    } else {
        format!("{}→{}", request.call_stack.join("→"), request.path)
    };
    let _ = writeln!(err, "\n? {} [{location}]", request.prompt.trim());

    let fields = fields(&request.schema);
    if fields.is_empty() {
        return AskOutcome::Unavailable("the question has no answerable fields".to_string());
    }
    let _ = writeln!(err, "  (blank cancels)");

    let mut answer = Map::new();

    for field in &fields {
        let label = match &field.description {
            Some(description) => format!("{} — {description}", field.name),
            None => field.name.clone(),
        };
        // The first prompt is not a retry: `retries_left` counts the
        // re-prompts a bad value earns, so the field is prompted
        // MAX_RETRIES + 1 times before the question is declined.
        let mut retries_left = MAX_RETRIES;
        loop {
            let _ = write!(err, "  {label} [{}]: ", hint(&field.kind));
            let _ = err.flush();
            let Some(line) = lines.next() else {
                // EOF: stdin closed mid-question.
                let _ = writeln!(err);
                return AskOutcome::Unavailable("stdin closed before the answer".to_string());
            };
            let text = line.trim();
            if text.is_empty() {
                if field.required {
                    return AskOutcome::Declined;
                }
                break;
            }
            match coerce(&field.kind, text) {
                Ok(value) => {
                    answer.insert(field.name.clone(), value);
                    break;
                }
                Err(problem) if retries_left > 0 => {
                    retries_left -= 1;
                    let _ = writeln!(err, "    {problem} — try again");
                }
                Err(problem) => {
                    let _ = writeln!(err, "    {problem}");
                    return AskOutcome::Declined;
                }
            }
        }
    }

    AskOutcome::Answered(Value::Object(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_carry_labels_types_and_requiredness() {
        let schema = json!({
            "type": "object",
            "required": ["repo"],
            "properties": {
                "repo": {"type": "string", "description": "Target repo"},
                "count": {"type": "integer"}
            }
        });
        let fields = fields(&schema);
        let repo = fields.iter().find(|f| f.name == "repo").unwrap();
        assert!(repo.required);
        assert_eq!(repo.description.as_deref(), Some("Target repo"));
        let count = fields.iter().find(|f| f.name == "count").unwrap();
        assert!(!count.required);
        assert!(matches!(count.kind, Kind::Integer));
    }

    #[test]
    fn an_enum_becomes_a_choice_listing_its_options() {
        let schema = json!({
            "type": "object",
            "properties": {"verdict": {"enum": ["ship", "hold"]}}
        });
        let field = &fields(&schema)[0];
        assert_eq!(hint(&field.kind), "ship | hold");
    }

    #[test]
    fn typed_lines_coerce_to_the_schema_type_not_to_strings() {
        // The answer is schema-validated by the pipeline, so "3" must
        // arrive as a number or a valid answer would be rejected.
        assert_eq!(coerce(&Kind::Integer, "3").unwrap(), json!(3));
        assert_eq!(coerce(&Kind::Number, "1.5").unwrap(), json!(1.5));
        assert_eq!(coerce(&Kind::Boolean, "yes").unwrap(), json!(true));
        assert_eq!(coerce(&Kind::Boolean, "N").unwrap(), json!(false));
        assert_eq!(coerce(&Kind::Text, "3").unwrap(), json!("3"));
    }

    #[test]
    fn a_choice_is_matched_case_insensitively_and_normalized() {
        let kind = Kind::Choice(vec!["ship".into(), "hold".into()]);
        assert_eq!(coerce(&kind, "SHIP").unwrap(), json!("ship"));
        let problem = coerce(&kind, "maybe").unwrap_err();
        assert!(problem.contains("ship, hold"), "{problem}");
    }

    #[test]
    fn bad_input_explains_the_expected_type() {
        assert!(coerce(&Kind::Integer, "abc")
            .unwrap_err()
            .contains("not an integer"));
        assert!(coerce(&Kind::Boolean, "maybe")
            .unwrap_err()
            .contains("not yes or no"));
    }

    fn request(schema: Value) -> AskRequest {
        AskRequest {
            path: graph_core::pipeline::StepPath::top("E0"),
            call_stack: Vec::new(),
            prompt: "Pick one".to_string(),
            schema,
        }
    }

    /// Run the form against scripted input, returning the outcome and
    /// everything written to the "terminal".
    fn fill(schema: Value, input: &[&str]) -> (AskOutcome, String) {
        let mut lines = input.iter().map(|s| (*s).to_string());
        let mut out: Vec<u8> = Vec::new();
        let outcome = fill_form(&request(schema), &mut lines, &mut out);
        (outcome, String::from_utf8(out).unwrap())
    }

    fn choice_schema() -> Value {
        json!({
            "type": "object",
            "required": ["verdict"],
            "properties": {"verdict": {"enum": ["ship", "hold"]}}
        })
    }

    #[test]
    fn a_bad_value_is_re_prompted_exactly_max_retries_times() {
        // The count the docs promise. Three bad values are each answered
        // with "try again"; the fourth prompt is the last one, so a bad
        // value there declines. Four prompts, three retries.
        let (outcome, shown) = fill(choice_schema(), &["a", "b", "c", "ship"]);
        assert!(matches!(outcome, AskOutcome::Answered(_)), "{shown}");
        assert_eq!(shown.matches("try again").count(), MAX_RETRIES);
        assert_eq!(shown.matches("verdict").count(), MAX_RETRIES + 1);
    }

    #[test]
    fn a_field_that_is_never_satisfied_declines_instead_of_spinning() {
        let (outcome, shown) = fill(choice_schema(), &["a", "b", "c", "d", "ship"]);
        assert!(matches!(outcome, AskOutcome::Declined), "{shown}");
        // The budget is spent on re-prompts, never on a silent extra read:
        // the trailing valid answer is not consumed.
        assert_eq!(shown.matches("verdict").count(), MAX_RETRIES + 1);
    }

    #[test]
    fn a_blank_line_on_a_required_field_declines_immediately() {
        let (outcome, shown) = fill(choice_schema(), &["", "ship"]);
        assert!(matches!(outcome, AskOutcome::Declined), "{shown}");
        assert!(!shown.contains("try again"), "{shown}");
    }

    #[test]
    fn a_blank_line_on_an_optional_field_omits_it_and_moves_on() {
        let schema = json!({
            "type": "object",
            "required": ["repo"],
            "properties": {
                "repo": {"type": "string"},
                "note": {"type": "string"}
            }
        });
        // Properties are walked in schema (sorted) order: note, then repo.
        let (outcome, shown) = fill(schema, &["", "graph"]);
        match outcome {
            AskOutcome::Answered(value) => {
                assert_eq!(value["repo"], "graph");
                assert!(value.get("note").is_none(), "{value}");
            }
            other => panic!("expected an answer, got {other:?} — {shown}"),
        }
    }

    #[test]
    fn stdin_closing_mid_question_is_unavailable_not_declined() {
        // Nobody refused — there was nobody there. The distinction is what
        // a plan's `reason` field reports.
        let (outcome, shown) = fill(choice_schema(), &[]);
        assert!(
            matches!(outcome, AskOutcome::Unavailable(_)),
            "{outcome:?} — {shown}"
        );
    }

    #[test]
    fn the_question_and_its_field_labels_reach_the_terminal() {
        let schema = json!({
            "type": "object",
            "required": ["verdict"],
            "properties": {
                "verdict": {"enum": ["ship", "hold"], "description": "Ship it?"}
            }
        });
        let (_, shown) = fill(schema, &["ship"]);
        assert!(shown.contains("Pick one"), "{shown}");
        assert!(shown.contains("Ship it?"), "{shown}");
        assert!(shown.contains("ship | hold"), "{shown}");
    }

    #[test]
    fn a_terminal_is_only_usable_when_both_streams_are_free() {
        assert!(TtyInterlocutor::available(false, true, true));
        // GRAPH_EVENTS=jsonl owns stderr: prompting on it would corrupt
        // the trace, so there is no interlocutor at all.
        assert!(!TtyInterlocutor::available(true, true, true));
        // Redirected stdin (`graph plan run < /dev/null`, CI) or a
        // captured stderr means the question could never be seen.
        assert!(!TtyInterlocutor::available(false, false, true));
        assert!(!TtyInterlocutor::available(false, true, false));
    }
}
