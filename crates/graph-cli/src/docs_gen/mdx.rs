use serde_yaml::Value as Yaml;
use std::fmt::Write as _;

pub fn frontmatter(out: &mut String, fields: &[(&str, String)], keywords: &[&str]) {
    out.push_str("---\n");
    for (key, value) in fields.iter().filter(|(_, value)| !value.is_empty()) {
        let _ = writeln!(out, "{key}: {}", yaml_string(value));
    }
    let quoted: Vec<String> = keywords
        .iter()
        .map(|keyword| yaml_string(keyword))
        .collect();
    let _ = writeln!(out, "keywords: [{}]\n---\n", quoted.join(", "));
}

pub fn yaml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn paragraphs(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| text_line(&paragraph.split_whitespace().collect::<Vec<_>>().join(" ")))
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn text_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    for ch in text.chars() {
        if ch == '`' {
            in_code = !in_code;
            out.push(ch);
            continue;
        }
        if in_code {
            out.push(ch);
            continue;
        }
        match ch {
            '{' => out.push_str("&#123;"),
            '}' => out.push_str("&#125;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn first_sentence(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.find(". ") {
        Some(end) => flat[..=end].to_string(),
        None => flat,
    }
}

pub fn attr(text: &str) -> String {
    text.replace('&', "&amp;").replace('"', "&quot;")
}

pub fn indented(out: &mut String, text: &str, indent: usize) {
    let pad = " ".repeat(indent);
    for line in text.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "{pad}{line}");
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Param,
    Response,
}

pub fn has_fields(schema: &Yaml) -> bool {
    schema.get("properties").is_some() || named_values(schema).is_some()
}

fn named_values(schema: &Yaml) -> Option<&Yaml> {
    schema
        .get("additionalProperties")
        .filter(|values| values.get("properties").is_some())
}

pub fn value_field(out: &mut String, schema: &Yaml) {
    field(out, "value", schema, false, Field::Response);
}

pub fn schema_fields(out: &mut String, schema: &Yaml, kind: Field, prefix: &str) {
    let Some(properties) = schema.get("properties").and_then(Yaml::as_mapping) else {
        if let Some(values) = named_values(schema) {
            let path = format!("{prefix}<name>");
            field(out, &path, values, false, kind);
            schema_fields(out, values, kind, &format!("{path}."));
        }
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Yaml::as_sequence)
        .map(|names| names.iter().filter_map(Yaml::as_str).collect())
        .unwrap_or_default();
    for (name, property) in properties {
        let Some(name) = name.as_str() else { continue };
        let path = format!("{prefix}{name}");
        field(out, &path, property, required.contains(&name), kind);
        if let Some((segment, nested)) = nested_schema(property) {
            schema_fields(out, nested, kind, &format!("{path}{segment}"));
        }
    }
}

fn field(out: &mut String, name: &str, property: &Yaml, required: bool, kind: Field) {
    let tag = match kind {
        Field::Param => "ParamField",
        Field::Response => "ResponseField",
    };
    let name_attr = match kind {
        Field::Param => "body",
        Field::Response => "name",
    };
    let _ = write!(
        out,
        "<{tag} {name_attr}=\"{}\" type=\"{}\"",
        attr(name),
        attr(&type_label(property))
    );
    if required {
        out.push_str(" required");
    }
    if let Some(default) = property.get("default") {
        let _ = write!(out, " default=\"{}\"", attr(&scalar(default)));
    }
    out.push_str(">\n");
    let mut body = Vec::new();
    if let Some(description) = property.get("description").and_then(Yaml::as_str) {
        body.push(paragraphs(description));
    }
    let values = enum_values(property);
    if !values.is_empty() {
        body.push(format!("Available options: {}", values.join(", ")));
    }
    indented(out, &body.join("\n\n"), 2);
    let _ = writeln!(out, "</{tag}>\n");
}

fn nested_schema(property: &Yaml) -> Option<(&'static str, &Yaml)> {
    if property.get("properties").is_some() {
        return Some((".", property));
    }
    if let Some(items) = property.get("items") {
        if items.get("properties").is_some() {
            return Some((".[].", items));
        }
    }
    named_values(property).map(|values| (".<name>.", values))
}

pub fn type_label(schema: &Yaml) -> String {
    if let Some(label) = schema.get("x-type").and_then(Yaml::as_str) {
        return label.to_string();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(options) = schema.get(key).and_then(Yaml::as_sequence) {
            let labels: Vec<String> = options.iter().map(type_label).collect();
            return labels.join(" | ");
        }
    }
    let base = match schema.get("type") {
        Some(Yaml::String(kind)) => single_type(kind, schema),
        Some(Yaml::Sequence(kinds)) => kinds
            .iter()
            .filter_map(Yaml::as_str)
            .map(|kind| single_type(kind, schema))
            .collect::<Vec<_>>()
            .join(" | "),
        _ if schema.get("properties").is_some() => "object".to_string(),
        _ => "any".to_string(),
    };
    if schema.get("enum").is_some() {
        format!("enum<{base}>")
    } else {
        base
    }
}

fn single_type(kind: &str, schema: &Yaml) -> String {
    match kind {
        "array" => match schema.get("items") {
            Some(items) => format!("{}[]", type_label(items)),
            None => "any[]".to_string(),
        },
        "object" => match schema.get("additionalProperties") {
            Some(values) if values.is_mapping() && values.get("properties").is_none() => {
                format!("map<string, {}>", type_label(values))
            }
            _ => "object".to_string(),
        },
        other => other.to_string(),
    }
}

fn enum_values(schema: &Yaml) -> Vec<String> {
    schema
        .get("enum")
        .and_then(Yaml::as_sequence)
        .map(|values| {
            values
                .iter()
                .map(|value| format!("`{}`", scalar(value)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn scalar(value: &Yaml) -> String {
    match value {
        Yaml::String(text) => text.clone(),
        Yaml::Null => "null".to_string(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

pub fn schema_from_example(example: &Yaml) -> Yaml {
    let mut schema = serde_yaml::Mapping::new();
    match example {
        Yaml::Mapping(map) => {
            schema.insert("type".into(), "object".into());
            let mut properties = serde_yaml::Mapping::new();
            for (key, value) in map {
                if key.as_str() == Some("…") {
                    continue;
                }
                properties.insert(key.clone(), schema_from_example(value));
            }
            if !properties.is_empty() {
                schema.insert("properties".into(), Yaml::Mapping(properties));
            }
        }
        Yaml::Sequence(items) => {
            schema.insert("type".into(), "array".into());
            if let Some(first) = items.first() {
                schema.insert("items".into(), schema_from_example(first));
            }
        }
        Yaml::Bool(_) => {
            schema.insert("type".into(), "boolean".into());
        }
        Yaml::Number(number) if number.is_f64() => {
            schema.insert("type".into(), "number".into());
        }
        Yaml::Number(_) => {
            schema.insert("type".into(), "integer".into());
        }
        Yaml::String(_) => {
            schema.insert("type".into(), "string".into());
        }
        _ => {}
    }
    Yaml::Mapping(schema)
}
