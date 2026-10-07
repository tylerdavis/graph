use super::form::{Field, FieldKind, Form};
use serde_json::{Map, Value};

pub fn answer_form(path: &str, question: &str, schema: &Value, answer: Option<&Value>) -> Form {
    let required = required(schema);
    let mut fields = Vec::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        let listed: Vec<&str> = schema
            .get("propertyOrder")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(required.iter().copied())
            .chain(properties.keys().map(String::as_str))
            .filter(|key| properties.contains_key(*key))
            .collect();
        let mut ordered: Vec<&str> = Vec::new();
        for key in listed {
            if !ordered.contains(&key) {
                ordered.push(key);
            }
        }
        for key in ordered {
            let prefill = answer.and_then(|answer| answer.get(key));
            fields.push(field(
                key,
                &properties[key],
                required.contains(&key),
                prefill,
            ));
        }
    }
    Form::new(format!("answer {path}"), question.trim(), fields)
}

pub fn read_answer(form: &mut Form, schema: &Value) -> Result<Value, Vec<String>> {
    let values = form.read()?;
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut answer = Map::new();
    let mut problems = Vec::new();
    for field in &mut form.fields {
        let Some(value) = values.get(&field.key) else {
            continue;
        };
        let property = properties
            .and_then(|properties| properties.get(&field.key))
            .unwrap_or(&Value::Null);
        match coerce(&field.label, property, value) {
            Ok(value) => {
                answer.insert(field.key.clone(), value);
            }
            Err(problem) => {
                field.error = Some(problem.clone());
                problems.push(problem);
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let answer = Value::Object(answer);
    if let Ok(validator) = jsonschema::validator_for(schema) {
        let problems: Vec<String> = validator
            .iter_errors(&answer)
            .map(|error| error.to_string())
            .collect();
        if !problems.is_empty() {
            return Err(problems);
        }
    }
    Ok(answer)
}

fn required(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn field(key: &str, property: &Value, required: bool, prefill: Option<&Value>) -> Field {
    let label = property.get("title").and_then(Value::as_str).unwrap_or(key);
    let declared = property.get("type").and_then(Value::as_str);
    let field = match choices(property) {
        Some(options) => Field::new(key, label, FieldKind::Text, false)
            .options(options)
            .strict(),
        None => match declared {
            Some("string" | "integer" | "number") => Field::new(key, label, FieldKind::Text, false),
            _ => Field::new(key, label, FieldKind::Json, true),
        },
    };
    let question = property
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let hint = match declared {
        Some("integer") => "a whole number",
        Some("number") => "a number",
        _ => "",
    };
    let default = prefill
        .or_else(|| property.get("default"))
        .filter(|value| !value.is_null())
        .map(|value| Value::String(display(value)));
    field
        .question(question)
        .required(required)
        .hint(hint)
        .value(default.as_ref())
}

fn choices(property: &Value) -> Option<Vec<String>> {
    if let Some(options) = property.get("enum").and_then(Value::as_array) {
        return Some(options.iter().map(display).collect());
    }
    (property.get("type").and_then(Value::as_str) == Some("boolean"))
        .then(|| vec!["true".to_string(), "false".to_string()])
}

fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn coerce(label: &str, property: &Value, value: &Value) -> Result<Value, String> {
    let Value::String(text) = value else {
        return Ok(value.clone());
    };
    let text = text.trim();
    if let Some(options) = property.get("enum").and_then(Value::as_array) {
        return options
            .iter()
            .find(|option| display(option) == text)
            .cloned()
            .ok_or_else(|| format!("{label} must be one of the listed choices"));
    }
    match property.get("type").and_then(Value::as_str) {
        Some("boolean") => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("{label} must be true or false")),
        },
        Some("integer") => text
            .parse::<i64>()
            .map(|n| Value::Number(n.into()))
            .map_err(|_| format!("{label} must be a whole number")),
        Some("number") => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| format!("{label} must be a number")),
        _ => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["status"],
            "properties": {
                "note": {"type": "string", "description": "Anything else?"},
                "limit": {"type": "integer", "default": 10},
                "status": {"enum": ["Todo", "In Progress", "Done"], "description": "Which status counts as started"},
                "strict": {"type": "boolean"},
                "priority": {"enum": [1, 2, 3]}
            }
        })
    }

    fn set(form: &mut Form, key: &str, text: &str) {
        let field = form.fields.iter_mut().find(|f| f.key == key).unwrap();
        field.set_text(text);
    }

    #[test]
    fn the_question_heads_a_form_with_one_field_per_property_required_first() {
        let form = answer_form("E2", "  Which status means started?\n", &schema(), None);
        assert_eq!(form.header, "Which status means started?");
        assert_eq!(form.title, "answer E2");
        let keys: Vec<&str> = form.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["status", "limit", "note", "priority", "strict"]);
        let status = &form.fields[0];
        assert!(status.required && status.strict);
        assert_eq!(
            status.options.as_deref(),
            Some(
                &[
                    "Done".to_string(),
                    "In Progress".to_string(),
                    "Todo".to_string()
                ][..]
            )
        );
        assert_eq!(
            status.question.as_deref(),
            Some("Which status counts as started")
        );
        assert_eq!(status.hint, None);
        assert_eq!(form.fields[1].text(), "10");
        assert_eq!(form.fields[1].hint.as_deref(), Some("a whole number"));
        assert!(form.fields[4].is_select());
    }

    #[test]
    fn a_property_order_puts_the_fields_in_that_order() {
        let schema = json!({
            "type": "object",
            "propertyOrder": ["zeta", "alpha"],
            "properties": {"alpha": {"type": "string"}, "beta": {"type": "string"}, "zeta": {"type": "string"}}
        });
        let form = answer_form("E1", "q", &schema, None);
        let keys: Vec<&str> = form.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["zeta", "alpha", "beta"]);
    }

    #[test]
    fn answers_read_back_typed_and_leave_empty_optional_fields_out() {
        let mut form = answer_form("E2", "q", &schema(), None);
        set(&mut form, "status", "In Progress");
        set(&mut form, "strict", "true");
        set(&mut form, "priority", "2");
        assert_eq!(
            read_answer(&mut form, &schema()).unwrap(),
            json!({"status": "In Progress", "limit": 10, "strict": true, "priority": 2})
        );
    }

    #[test]
    fn bad_values_mark_their_fields_and_missing_required_ones_fail() {
        let mut form = answer_form("E2", "q", &schema(), None);
        set(&mut form, "limit", "ten");
        let problems = read_answer(&mut form, &schema()).unwrap_err();
        assert_eq!(problems, ["status is required"]);

        set(&mut form, "status", "Todo");
        let problems = read_answer(&mut form, &schema()).unwrap_err();
        assert_eq!(problems, ["limit must be a whole number"]);
        let limit = form.fields.iter().find(|f| f.key == "limit").unwrap();
        assert_eq!(limit.error.as_deref(), Some("limit must be a whole number"));
    }

    #[test]
    fn schema_constraints_the_fields_cannot_express_are_still_checked() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string", "minLength": 3}}
        });
        let mut form = answer_form("E1", "q", &schema, None);
        set(&mut form, "name", "ab");
        let problems = read_answer(&mut form, &schema).unwrap_err();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("shorter than 3"), "{problems:?}");
    }
}
