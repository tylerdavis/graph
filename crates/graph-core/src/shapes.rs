//! JSON shape inference for the observed-shape cache: turn an actual tool
//! result into a compact JSON Schema plus a truncated example the planner
//! can reason over.

use serde_json::{json, Map, Value};

const MAX_STRING_LEN: usize = 120;
const MAX_ARRAY_EXAMPLES: usize = 2;
const MAX_DEPTH: usize = 8;

/// Infer a JSON Schema from an observed value. Arrays are described by
/// their first element; unions beyond that are out of scope for a cache
/// whose job is "good enough for the planner to write `{{Ex.path}}` refs".
pub fn infer_schema(value: &Value) -> Value {
    infer_at(value, 0)
}

fn infer_at(value: &Value, depth: usize) -> Value {
    if depth >= MAX_DEPTH {
        return json!({});
    }
    match value {
        Value::Null => json!({"type": "null"}),
        Value::Bool(_) => json!({"type": "boolean"}),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                json!({"type": "integer"})
            } else {
                json!({"type": "number"})
            }
        }
        Value::String(_) => json!({"type": "string"}),
        Value::Array(items) => match items.first() {
            Some(first) => json!({"type": "array", "items": infer_at(first, depth + 1)}),
            None => json!({"type": "array"}),
        },
        Value::Object(map) => {
            let mut properties = Map::new();
            for (key, child) in map {
                properties.insert(key.clone(), infer_at(child, depth + 1));
            }
            json!({"type": "object", "properties": properties})
        }
    }
}

/// Shrink a value into an example small enough to embed in a prompt:
/// long strings truncated, arrays sampled to their first elements.
pub fn truncate_example(value: &Value) -> Value {
    truncate_at(value, 0)
}

fn truncate_at(value: &Value, depth: usize) -> Value {
    if depth >= MAX_DEPTH {
        return json!("…");
    }
    match value {
        Value::String(s) if s.chars().count() > MAX_STRING_LEN => {
            let truncated: String = s.chars().take(MAX_STRING_LEN).collect();
            Value::String(format!("{truncated}…"))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(MAX_ARRAY_EXAMPLES)
                .map(|item| truncate_at(item, depth + 1))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), truncate_at(v, depth + 1)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_nested_schema_from_result() {
        let value = json!({
            "values": [{"id": "abc", "count": 3, "merged": true, "score": 1.5}],
            "total": 1,
        });
        let schema = infer_schema(&value);
        assert_eq!(schema["properties"]["values"]["type"], "array");
        assert_eq!(
            schema["properties"]["values"]["items"]["properties"]["id"]["type"],
            "string"
        );
        assert_eq!(
            schema["properties"]["values"]["items"]["properties"]["score"]["type"],
            "number"
        );
        assert_eq!(schema["properties"]["total"]["type"], "integer");
    }

    #[test]
    fn truncates_long_strings_and_samples_arrays() {
        let long = "x".repeat(500);
        let value = json!({"body": long, "items": [1, 2, 3, 4, 5]});
        let example = truncate_example(&value);
        assert!(example["body"].as_str().unwrap().chars().count() <= MAX_STRING_LEN + 1);
        assert_eq!(
            example["items"].as_array().unwrap().len(),
            MAX_ARRAY_EXAMPLES
        );
    }
}
