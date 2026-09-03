use super::form::{Field, FieldKind, Form};
use graph_core::pipeline::authoring;
use graph_core::pipeline::body::{parse_branch, Branch};
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::plan::Step;
use graph_core::ToolDef;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepTarget {
    Top {
        id: String,
    },
    Body {
        step: String,
        body: String,
        position: Option<usize>,
    },
    New {
        index: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    Metadata,
    Step(StepTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    pub target: EditTarget,
    pub values: Map<String, Value>,
}

const INPUT_PREFIX: &str = "in:";
const HINT_WIDTH: usize = 72;

pub fn blank_step(id: &str) -> Step {
    Step {
        id: id.to_string(),
        tool_name: String::new(),
        input: Map::new(),
        reasoning: None,
    }
}

pub fn step_at(doc: &PlanDoc, target: &StepTarget) -> Option<(Step, bool)> {
    match target {
        StepTarget::New { .. } => Some((blank_step(""), true)),
        StepTarget::Top { id } => doc
            .steps
            .iter()
            .find(|step| &step.id == id)
            .map(|step| (step.clone(), true)),
        StepTarget::Body {
            step,
            body,
            position,
        } => {
            let owner = doc.steps.iter().find(|s| &s.id == step)?;
            let raw = owner.input.get(body)?;
            match (parse_branch(body, raw).ok()?, position) {
                (Branch::Steps(steps), Some(index)) => {
                    steps.into_iter().nth(*index).map(|step| (step, true))
                }
                (Branch::Call(call), None) => Some((
                    Step {
                        id: String::new(),
                        tool_name: call.tool_name,
                        input: call.input,
                        reasoning: call.reasoning,
                    },
                    false,
                )),
                _ => None,
            }
        }
    }
}

pub fn step_form(label: &str, step: &Step, has_id: bool, tools: &[ToolDef]) -> Form {
    let def = tools.iter().find(|t| t.name == step.tool_name);
    let mut fields = Vec::new();
    if has_id {
        fields.push(
            Field::new("id", "id", FieldKind::Text, false)
                .required(true)
                .hint("letters, digits, _ — renaming rewrites downstream references")
                .value(Some(&Value::String(step.id.clone()))),
        );
    }
    fields.push(
        Field::new("tool", "tool", FieldKind::Text, false)
            .required(true)
            .hint("type to filter the catalog — changing it reloads the input fields")
            .value(Some(&Value::String(step.tool_name.clone())))
            .options(tools.iter().map(|t| t.name.clone()).collect()),
    );
    fields.push(
        Field::new("reasoning", "reasoning", FieldKind::Text, true)
            .hint("why this step exists and what it should produce")
            .value(
                step.reasoning
                    .as_ref()
                    .map(|r| Value::String(r.clone()))
                    .as_ref(),
            ),
    );

    let schema = def.map(|d| &d.input_schema);
    let properties = schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object);
    let required: Vec<&str> = schema
        .and_then(|s| s.get("required"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut covered: Vec<String> = Vec::new();
    if let Some(properties) = properties {
        let ordered = required
            .iter()
            .copied()
            .filter(|key| properties.contains_key(*key))
            .chain(
                properties
                    .keys()
                    .map(String::as_str)
                    .filter(|key| !required.contains(key)),
            );
        for key in ordered {
            let property = &properties[key];
            let existing = step.input.get(key);
            let (kind, multiline) = input_field_kind(property.get("type"), existing);
            let hint = property
                .get("description")
                .and_then(Value::as_str)
                .map(clip_hint)
                .or_else(|| enum_hint(property))
                .unwrap_or_default();
            fields.push(
                Field::new(&format!("{INPUT_PREFIX}{key}"), key, kind, multiline)
                    .required(required.contains(&key))
                    .hint(hint)
                    .value(existing),
            );
            covered.push(key.to_string());
        }
    }
    for (key, value) in &step.input {
        if covered.contains(key) {
            continue;
        }
        let (kind, multiline) = input_field_kind(None, Some(value));
        fields.push(
            Field::new(&format!("{INPUT_PREFIX}{key}"), key, kind, multiline).value(Some(value)),
        );
    }
    Form::new(format!("edit {label}"), "", fields)
}

pub fn reload_step_form(form: &Form, label: &str, tools: &[ToolDef]) -> Form {
    let text = |key: &str| form.fields.iter().find(|f| f.key == key).map(Field::text);
    let has_id = form.fields.iter().any(|f| f.key == "id");
    let mut input = Map::new();
    for field in &form.fields {
        let Some(key) = field.key.strip_prefix(INPUT_PREFIX) else {
            continue;
        };
        match field.read() {
            Ok(Some(value)) => {
                input.insert(key.to_string(), value);
            }
            Ok(None) => {}
            Err(_) => {
                input.insert(key.to_string(), Value::String(field.text()));
            }
        }
    }
    let step = Step {
        id: text("id").unwrap_or_default(),
        tool_name: text("tool").unwrap_or_default(),
        input,
        reasoning: text("reasoning").filter(|r| !r.trim().is_empty()),
    };
    let mut rebuilt = step_form(label, &step, has_id, tools);
    let after_tool = rebuilt
        .fields
        .iter()
        .position(|f| f.key == "tool")
        .map(|i| i + 1)
        .unwrap_or(0);
    rebuilt.set_focus(form.focused.max(after_tool));
    rebuilt.mark_reloaded();
    rebuilt
}

fn input_field_kind(declared: Option<&Value>, existing: Option<&Value>) -> (FieldKind, bool) {
    match declared.and_then(Value::as_str) {
        Some("string") => (FieldKind::Text, true),
        Some("object") | Some("array") => (FieldKind::Json, true),
        Some("number") | Some("integer") | Some("boolean") => (FieldKind::Json, false),
        _ => match existing {
            Some(Value::String(_)) => (FieldKind::Text, true),
            Some(Value::Null) | None => (FieldKind::Auto, true),
            Some(Value::Object(_)) | Some(Value::Array(_)) => (FieldKind::Json, true),
            Some(_) => (FieldKind::Json, false),
        },
    }
}

fn enum_hint(property: &Value) -> Option<String> {
    let options: Vec<String> = property
        .get("enum")?
        .as_array()?
        .iter()
        .map(|option| match option {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    (!options.is_empty()).then(|| clip_hint(&format!("one of: {}", options.join(", "))))
}

fn clip_hint(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default();
    if first.chars().count() <= HINT_WIDTH {
        return first.to_string();
    }
    let mut clipped: String = first.chars().take(HINT_WIDTH - 1).collect();
    clipped.push('…');
    clipped
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishMode {
    Solver,
    Output,
    Silent,
}

impl FinishMode {
    pub fn label(self) -> &'static str {
        match self {
            FinishMode::Solver => "solver",
            FinishMode::Output => "output",
            FinishMode::Silent => "silent",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "solver" => Some(FinishMode::Solver),
            "output" => Some(FinishMode::Output),
            "silent" => Some(FinishMode::Silent),
            _ => None,
        }
    }

    fn of(doc: &PlanDoc) -> Self {
        if doc.solver.is_some() {
            FinishMode::Solver
        } else if doc.output.is_some() {
            FinishMode::Output
        } else {
            FinishMode::Silent
        }
    }
}

const FINISH_KEY: &str = "finish";

fn metadata_fields(mode: FinishMode) -> Vec<Field> {
    let mut fields = vec![
        Field::new("identifier", "identifier", FieldKind::Text, false)
            .required(true)
            .hint("letters, digits, _, - — changing it makes this a new plan file"),
        Field::new("name", "name", FieldKind::Text, false)
            .required(true)
            .hint("display name"),
        Field::new("description", "description", FieldKind::Text, true)
            .hint("what the plan does — shown in the catalog and used for routing"),
        Field::new("exemplars", "exemplars", FieldKind::Lines, true)
            .hint("example requests the plan should handle"),
        Field::new(
            "requires_servers",
            "requires servers",
            FieldKind::Lines,
            true,
        )
        .hint("MCP servers the plan needs configured"),
        Field::new("input_schema", "input schema", FieldKind::Json, true)
            .hint("JSON Schema for {{input.*}} — empty for a plan that takes no input"),
        Field::new(FINISH_KEY, "finish", FieldKind::Text, false)
            .required(true)
            .hint("solver: an LLM report · output: a rendered template map · silent: side effects only")
            .options(
                [FinishMode::Solver, FinishMode::Output, FinishMode::Silent]
                    .iter()
                    .map(|mode| mode.label().to_string())
                    .collect(),
            ),
    ];
    match mode {
        FinishMode::Solver => fields.extend([
            Field::new(
                "solver_query",
                "solver: query to answer",
                FieldKind::Text,
                true,
            )
            .required(true)
            .hint("the question the solver answers from the step results"),
            Field::new(
                "solver_system_prompt",
                "solver: system prompt",
                FieldKind::Text,
                true,
            )
            .hint("extra guidance for the solver"),
            Field::new("solver_data", "solver: data", FieldKind::Json, true)
                .hint("template map of step results the solver reads — empty means every step"),
        ]),
        FinishMode::Output => fields.push(
            Field::new("output", "output", FieldKind::Json, true)
                .required(true)
                .hint("template map rendered as the plan's JSON result"),
        ),
        FinishMode::Silent => {}
    }
    fields
}

fn metadata_texts(doc: &PlanDoc) -> Map<String, Value> {
    let mut texts = Map::new();
    let mut put = |key: &str, text: String| {
        texts.insert(key.to_string(), Value::String(text));
    };
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
    put("identifier", doc.identifier.clone());
    put("name", doc.name.clone());
    put("description", doc.description.clone());
    put("exemplars", doc.exemplars.join("\n"));
    put("requires_servers", doc.requires_servers.join("\n"));
    if let Some(schema) = &doc.input_schema {
        put("input_schema", pretty(schema));
    }
    put(FINISH_KEY, FinishMode::of(doc).label().to_string());
    if let Some(solver) = &doc.solver {
        put("solver_query", solver.query_to_answer.clone());
        if let Some(prompt) = &solver.system_prompt {
            put("solver_system_prompt", prompt.clone());
        }
        if !solver.data.is_empty() {
            put("solver_data", pretty(&Value::Object(solver.data.clone())));
        }
    }
    if let Some(output) = &doc.output {
        put("output", pretty(&Value::Object(output.clone())));
    }
    texts
}

fn assemble_metadata_form(mode: FinishMode, texts: &Map<String, Value>) -> Form {
    let mut fields = metadata_fields(mode);
    for field in &mut fields {
        if let Some(text) = texts.get(&field.key).and_then(Value::as_str) {
            field.set_text(text);
        }
    }
    Form::new("edit plan metadata", "", fields)
}

pub fn metadata_form(doc: &PlanDoc) -> Form {
    assemble_metadata_form(FinishMode::of(doc), &metadata_texts(doc))
}

pub fn reload_metadata_form(form: &Form) -> Form {
    let mut texts = Map::new();
    for field in &form.fields {
        texts.insert(field.key.clone(), Value::String(field.text()));
    }
    let mode = texts
        .get(FINISH_KEY)
        .and_then(Value::as_str)
        .and_then(FinishMode::parse)
        .unwrap_or(FinishMode::Silent);
    let mut rebuilt = assemble_metadata_form(mode, &texts);
    rebuilt.set_focus(form.focused);
    rebuilt.mark_reloaded();
    rebuilt
}

impl PendingEdit {
    pub fn apply(&self, doc: &mut PlanDoc) -> Result<Value, Value> {
        match &self.target {
            EditTarget::Metadata => authoring::patch_metadata(doc, &self.metadata_patch()),
            EditTarget::Step(StepTarget::New { index }) => {
                let step = Step {
                    id: self.text("id").unwrap_or_default(),
                    tool_name: self.text("tool").unwrap_or_default(),
                    input: self.input_object()?,
                    reasoning: self.text("reasoning"),
                };
                let index = (*index).min(doc.steps.len());
                let id = step.id.clone();
                doc.steps.insert(index, step);
                Ok(json!({"ok": true, "id": id, "index": index}))
            }
            EditTarget::Step(StepTarget::Top { id }) => {
                let mut patch = json!({
                    "id": id,
                    "toolName": self.values.get("tool").cloned().unwrap_or(Value::Null),
                    "input": Value::Object(self.input_object()?),
                    "reasoning": self.text("reasoning").unwrap_or_default(),
                });
                if let Some(new_id) = self.text("id") {
                    patch["newId"] = Value::String(new_id);
                }
                authoring::patch_update_step(doc, &patch)
            }
            EditTarget::Step(StepTarget::Body {
                step,
                body,
                position,
            }) => {
                let input = self.input_object()?;
                let index = authoring::position_of(step, &doc.steps)?;
                let Some(slot) = doc.steps[index].input.get_mut(body) else {
                    return Err(json!({"error": format!("step {step} has no `{body}` body")}));
                };
                let raw = match position {
                    Some(p) => slot.as_array_mut().and_then(|items| items.get_mut(*p)),
                    None => Some(slot),
                };
                let Some(raw) = raw.and_then(Value::as_object_mut) else {
                    return Err(json!({
                        "error": format!("the `{body}` body of {step} no longer has this step")
                    }));
                };
                if let Some(id) = self.text("id") {
                    raw.insert("id".to_string(), Value::String(id));
                }
                let tool_key = if raw.contains_key("toolName") {
                    "toolName"
                } else {
                    "tool_name"
                };
                raw.insert(
                    tool_key.to_string(),
                    self.values.get("tool").cloned().unwrap_or(Value::Null),
                );
                raw.insert("input".to_string(), Value::Object(input));
                match self.text("reasoning") {
                    Some(reasoning) => {
                        raw.insert("reasoning".to_string(), Value::String(reasoning));
                    }
                    None => {
                        raw.remove("reasoning");
                    }
                }
                Ok(json!({"ok": true, "step": step, "body": body}))
            }
        }
    }

    fn text(&self, key: &str) -> Option<String> {
        self.values
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn input_object(&self) -> Result<Map<String, Value>, Value> {
        let mut input = Map::new();
        for (key, value) in &self.values {
            if let Some(name) = key.strip_prefix(INPUT_PREFIX) {
                input.insert(name.to_string(), value.clone());
            }
        }
        Ok(input)
    }

    fn metadata_patch(&self) -> Value {
        let get = |key: &str| self.values.get(key).cloned();
        let mode = self
            .text(FINISH_KEY)
            .and_then(|text| FinishMode::parse(&text))
            .unwrap_or(FinishMode::Silent);
        let mut finish = Map::new();
        match mode {
            FinishMode::Solver => {
                let mut solver = json!({
                    "queryToAnswer": get("solver_query").unwrap_or_else(|| Value::String(String::new()))
                });
                if let Some(prompt) = get("solver_system_prompt") {
                    solver["systemPrompt"] = prompt;
                }
                if let Some(data) = get("solver_data") {
                    solver["data"] = data;
                }
                finish.insert("solver".to_string(), solver);
            }
            FinishMode::Output => {
                finish.insert(
                    "output".to_string(),
                    get("output").unwrap_or_else(|| Value::Object(Map::new())),
                );
            }
            FinishMode::Silent => {}
        }
        json!({
            "identifier": get("identifier").unwrap_or(Value::Null),
            "name": get("name").unwrap_or(Value::Null),
            "description": get("description").unwrap_or_else(|| Value::String(String::new())),
            "exemplars": get("exemplars").unwrap_or_else(|| Value::Array(Vec::new())),
            "requires_servers": get("requires_servers").unwrap_or_else(|| Value::Array(Vec::new())),
            "input_schema": get("input_schema").unwrap_or(Value::Null),
            "finish": Value::Object(finish),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui_textarea::TextArea;

    fn doc() -> PlanDoc {
        serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
exemplars: ["one", "two"]
steps:
  - id: E0
    tool_name: t__search
    input: { query: x, limit: 5 }
    reasoning: find things
  - id: E1
    tool_name: map
    input:
      over: "{{E0.items}}"
      do:
        - id: E2
          tool_name: t__fetch
          input: { url: "{{item.url}}" }
  - id: E3
    tool_name: decide
    input:
      if: { value: "{{E0.count}}", op: gt, to: 0 }
      then: { tool_name: t__notify, input: { message: "{{E0.query}}" } }
  - id: E4
    tool_name: exit
    input: { status: success, when: { value: "{{E0.count}}", op: eq, to: 0 } }
solver:
  queryToAnswer: what happened with {{E0.query}}?
"#,
        )
        .unwrap()
    }

    fn search_def() -> ToolDef {
        ToolDef {
            name: "t__search".to_string(),
            description: String::new(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "limit": {"type": "integer", "description": "max results"},
                    "query": {"type": "string", "description": "search text"},
                    "tags": {"type": "array"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        }
    }

    fn set(form: &mut Form, key: &str, text: &str) {
        let field = form.fields.iter_mut().find(|f| f.key == key).unwrap();
        field.textarea = TextArea::from(text.lines().map(str::to_string).collect::<Vec<_>>());
    }

    fn keys(form: &Form) -> Vec<&str> {
        form.fields.iter().map(|f| f.key.as_str()).collect()
    }

    #[test]
    fn step_form_lists_schema_fields_required_first_then_existing_extras() {
        let doc = doc();
        let target = StepTarget::Top { id: "E0".into() };
        let (step, has_id) = step_at(&doc, &target).unwrap();
        let form = step_form("step E0", &step, has_id, &[search_def()]);
        assert_eq!(
            keys(&form),
            ["id", "tool", "reasoning", "in:query", "in:limit", "in:tags"]
        );
        assert!(form.fields[1].is_select());
        assert_eq!(form.fields[1].matches(), ["t__search"]);
        let query = &form.fields[3];
        assert!(query.required);
        assert_eq!(query.kind, FieldKind::Text);
        assert_eq!(query.text(), "x");
        assert_eq!(query.hint.as_deref(), Some("search text"));
        let limit = &form.fields[4];
        assert_eq!((limit.kind, limit.multiline), (FieldKind::Json, false));
        assert_eq!(limit.text(), "5");
        assert_eq!(form.fields[5].text(), "");
    }

    #[test]
    fn unknown_tools_get_fields_from_the_existing_input() {
        let doc = doc();
        let (step, _) = step_at(&doc, &StepTarget::Top { id: "E0".into() }).unwrap();
        let form = step_form("step E0", &step, true, &[]);
        assert_eq!(
            keys(&form),
            ["id", "tool", "reasoning", "in:limit", "in:query"]
        );
        assert_eq!(form.fields[3].kind, FieldKind::Json);
        assert_eq!(form.fields[4].kind, FieldKind::Text);
    }

    #[test]
    fn control_step_fields_come_from_the_control_schema() {
        let doc = doc();
        let defs = graph_core::pipeline::control_step_defs();
        let (step, _) = step_at(&doc, &StepTarget::Top { id: "E4".into() }).unwrap();
        let form = step_form("step E4", &step, true, &defs);
        assert_eq!(
            keys(&form),
            [
                "id",
                "tool",
                "reasoning",
                "in:status",
                "in:infer",
                "in:message",
                "in:model",
                "in:when"
            ],
        );
        let when = form.fields.iter().find(|f| f.key == "in:when").unwrap();
        assert_eq!(when.kind, FieldKind::Json);
        assert!(when.text().contains("\"op\": \"eq\""));
        let status = form.fields.iter().find(|f| f.key == "in:status").unwrap();
        assert_eq!(status.hint.as_deref(), Some("one of: success, error"));
        assert!(status.required);
    }

    #[test]
    fn top_level_edits_rename_and_rewrite_references() {
        let doc = doc();
        let target = StepTarget::Top { id: "E0".into() };
        let (step, has_id) = step_at(&doc, &target).unwrap();
        let mut form = step_form("step E0", &step, has_id, &[search_def()]);
        set(&mut form, "id", "search");
        set(&mut form, "in:query", "y");
        set(&mut form, "in:limit", "");
        set(&mut form, "in:tags", "[\"a\"]");
        set(&mut form, "reasoning", "");
        let values = form.read().unwrap();
        let edit = PendingEdit {
            target: EditTarget::Step(target),
            values,
        };
        let mut edited = doc.clone();
        edit.apply(&mut edited).unwrap();
        let step = &edited.steps[0];
        assert_eq!(step.id, "search");
        assert_eq!(step.reasoning, None);
        assert_eq!(
            Value::Object(step.input.clone()),
            json!({"query": "y", "tags": ["a"]})
        );
        assert_eq!(edited.steps[1].input["over"], json!("{{search.items}}"));
        assert_eq!(
            edited.solver.as_ref().unwrap().query_to_answer,
            "what happened with {{search.query}}?"
        );
        assert!(authoring::static_problems(&edited).is_empty());
    }

    #[test]
    fn body_edits_write_back_into_the_owning_step() {
        let doc = doc();
        let target = StepTarget::Body {
            step: "E1".into(),
            body: "do".into(),
            position: Some(0),
        };
        let (step, has_id) = step_at(&doc, &target).unwrap();
        assert!(has_id);
        assert_eq!(step.tool_name, "t__fetch");
        let mut form = step_form("step E2", &step, has_id, &[]);
        set(&mut form, "tool", "t__get");
        set(&mut form, "reasoning", "fetch each");
        let edit = PendingEdit {
            target: EditTarget::Step(target),
            values: form.read().unwrap(),
        };
        let mut edited = doc.clone();
        edit.apply(&mut edited).unwrap();
        assert_eq!(
            edited.steps[1].input["do"],
            json!([{"id": "E2", "tool_name": "t__get", "input": {"url": "{{item.url}}"}, "reasoning": "fetch each"}])
        );

        let call = StepTarget::Body {
            step: "E3".into(),
            body: "then".into(),
            position: None,
        };
        let (step, has_id) = step_at(&doc, &call).unwrap();
        assert!(!has_id);
        let mut form = step_form("then branch of E3", &step, has_id, &[]);
        assert_eq!(keys(&form), ["tool", "reasoning", "in:message"]);
        set(&mut form, "in:message", "hi");
        let edit = PendingEdit {
            target: EditTarget::Step(call),
            values: form.read().unwrap(),
        };
        let mut edited = doc.clone();
        edit.apply(&mut edited).unwrap();
        assert_eq!(
            edited.steps[2].input["then"],
            json!({"tool_name": "t__notify", "input": {"message": "hi"}})
        );

        let gone = StepTarget::Body {
            step: "E1".into(),
            body: "do".into(),
            position: Some(4),
        };
        assert!(step_at(&doc, &gone).is_none());
    }

    #[test]
    fn reloading_after_a_tool_change_keeps_typed_values() {
        let doc = doc();
        let fetch = ToolDef {
            name: "t__fetch".to_string(),
            description: String::new(),
            input_schema: json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": {"type": "string"},
                    "limit": {"type": "integer"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        };
        let tools = vec![search_def(), fetch];
        let (step, has_id) = step_at(&doc, &StepTarget::Top { id: "E0".into() }).unwrap();
        let mut form = step_form("step E0", &step, has_id, &tools);
        set(&mut form, "reasoning", "kept");
        set(&mut form, "in:limit", "9");
        set(&mut form, "in:tags", "{oops");
        set(&mut form, "tool", "t__fetch");
        form.set_focus(2);

        let reloaded = reload_step_form(&form, "step E0", &tools);
        assert_eq!(
            keys(&reloaded),
            [
                "id",
                "tool",
                "reasoning",
                "in:url",
                "in:limit",
                "in:query",
                "in:tags"
            ]
        );
        let text = |key: &str| {
            reloaded
                .fields
                .iter()
                .find(|f| f.key == key)
                .unwrap()
                .text()
        };
        assert_eq!(text("id"), "E0");
        assert_eq!(text("tool"), "t__fetch");
        assert_eq!(text("reasoning"), "kept");
        assert_eq!(text("in:url"), "");
        assert!(
            reloaded
                .fields
                .iter()
                .find(|f| f.key == "in:url")
                .unwrap()
                .required
        );
        assert_eq!(text("in:limit"), "9");
        assert_eq!(text("in:query"), "x");
        assert_eq!(text("in:tags"), "{oops", "unparsable text survives as text");
        assert_eq!(reloaded.focused, 2);
    }

    #[test]
    fn a_new_step_is_inserted_at_its_index_on_apply() {
        let doc = doc();
        let mut form = step_form("new step", &blank_step("E9"), true, &[search_def()]);
        assert_eq!(keys(&form), ["id", "tool", "reasoning"]);
        assert_eq!(form.fields[0].text(), "E9");
        assert!(
            form.fields[1].matches().contains(&"t__search"),
            "empty tool lists the catalog"
        );
        let problems = form.read().unwrap_err();
        assert_eq!(problems, vec!["tool is required".to_string()]);

        set(&mut form, "tool", "t__search");
        let mut form = reload_step_form(&form, "new step", &[search_def()]);
        assert_eq!(
            keys(&form),
            ["id", "tool", "reasoning", "in:query", "in:limit", "in:tags"]
        );
        set(&mut form, "in:query", "{{E0.query}} again");
        let edit = PendingEdit {
            target: EditTarget::Step(StepTarget::New { index: 1 }),
            values: form.read().unwrap(),
        };
        let mut edited = doc.clone();
        edit.apply(&mut edited).unwrap();
        assert_eq!(edited.steps.len(), doc.steps.len() + 1);
        assert_eq!(edited.steps[1].id, "E9");
        assert_eq!(edited.steps[1].tool_name, "t__search");
        assert_eq!(edited.steps[1].input["query"], json!("{{E0.query}} again"));
        assert!(authoring::static_problems(&edited).is_empty());

        let past_the_end = PendingEdit {
            target: EditTarget::Step(StepTarget::New { index: 99 }),
            values: edit.values.clone(),
        };
        let mut edited = doc.clone();
        past_the_end.apply(&mut edited).unwrap();
        assert_eq!(edited.steps.last().unwrap().id, "E9");
    }

    #[test]
    fn metadata_form_round_trips_and_switches_the_finish() {
        let doc = doc();
        let mut form = metadata_form(&doc);
        assert_eq!(
            keys(&form),
            [
                "identifier",
                "name",
                "description",
                "exemplars",
                "requires_servers",
                "input_schema",
                "finish",
                "solver_query",
                "solver_system_prompt",
                "solver_data"
            ]
        );
        let text =
            |form: &Form, key: &str| form.fields.iter().find(|f| f.key == key).unwrap().text();
        assert_eq!(text(&form, "exemplars"), "one\ntwo");
        assert_eq!(text(&form, "finish"), "solver");
        assert!(form
            .fields
            .iter()
            .find(|f| f.key == "finish")
            .unwrap()
            .is_select());
        assert_eq!(
            text(&form, "solver_query"),
            "what happened with {{E0.query}}?"
        );

        set(&mut form, "name", "Renamed");
        set(&mut form, "exemplars", "one\n\nthree");
        set(&mut form, "input_schema", "{\"type\": \"object\"}");
        set(&mut form, "solver_system_prompt", "be brief");
        let edit = PendingEdit {
            target: EditTarget::Metadata,
            values: form.read().unwrap(),
        };
        let mut edited = doc.clone();
        edit.apply(&mut edited).unwrap();
        assert_eq!(edited.name, "Renamed");
        assert_eq!(edited.exemplars, vec!["one", "three"]);
        assert_eq!(edited.input_schema, Some(json!({"type": "object"})));
        let solver = edited.solver.as_ref().unwrap();
        assert_eq!(solver.query_to_answer, "what happened with {{E0.query}}?");
        assert_eq!(solver.system_prompt.as_deref(), Some("be brief"));
        assert!(edited.output.is_none());

        set(&mut form, "finish", "output");
        form.set_focus(3);
        let mut form = reload_metadata_form(&form);
        assert_eq!(
            keys(&form),
            [
                "identifier",
                "name",
                "description",
                "exemplars",
                "requires_servers",
                "input_schema",
                "finish",
                "output"
            ]
        );
        assert_eq!(text(&form, "name"), "Renamed", "common fields carry over");
        assert_eq!(form.focused, 3);
        let problems = form.read().unwrap_err();
        assert_eq!(problems, vec!["output is required".to_string()]);
        set(&mut form, "output", "{\"count\": \"{{E0.count}}\"}");
        let edit = PendingEdit {
            target: EditTarget::Metadata,
            values: form.read().unwrap(),
        };
        let mut edited = doc.clone();
        edit.apply(&mut edited).unwrap();
        assert!(edited.solver.is_none());
        assert_eq!(
            edited.output.as_ref().map(|o| Value::Object(o.clone())),
            Some(json!({"count": "{{E0.count}}"}))
        );

        set(&mut form, "finish", "silent");
        let mut form = reload_metadata_form(&form);
        assert_eq!(keys(&form).last(), Some(&"finish"));
        let edit = PendingEdit {
            target: EditTarget::Metadata,
            values: form.read().unwrap(),
        };
        let mut edited = doc.clone();
        edit.apply(&mut edited).unwrap();
        assert!(edited.solver.is_none() && edited.output.is_none());
    }
}
