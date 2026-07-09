//! Tool/plan input resolution: a JSON document (inline, `@file`, or `-` for
//! stdin) layered under repeatable `--input key=value` overrides.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::Read;

/// Resolve the effective input object.
///
/// `document`: `{…}` inline JSON, `@path` to read a JSON file, or `-` to
/// read JSON from stdin. `pairs` are `key=value` overrides applied on top
/// (values parse as JSON when possible, else strings).
pub fn resolve_input(document: Option<&str>, pairs: &[String]) -> Result<Value> {
    let mut base = match document {
        None => Value::Object(Default::default()),
        Some("-") => {
            let mut raw = String::new();
            std::io::stdin()
                .read_to_string(&mut raw)
                .context("reading input JSON from stdin")?;
            parse_document(raw.trim(), "stdin")?
        }
        Some(doc) if doc.starts_with('@') => {
            let path = &doc[1..];
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading input file {path}"))?;
            parse_document(raw.trim(), path)?
        }
        Some(doc) => parse_document(doc, "argument")?,
    };

    let map = base
        .as_object_mut()
        .expect("parse_document guarantees an object");
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            bail!("--input must be key=value, got: {pair}");
        };
        let parsed =
            serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
        map.insert(key.to_string(), parsed);
    }
    Ok(base)
}

fn parse_document(raw: &str, source: &str) -> Result<Value> {
    if raw.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let value: Value = serde_json::from_str(raw)
        .with_context(|| format!("input from {source} is not valid JSON"))?;
    if !value.is_object() {
        bail!("input from {source} must be a JSON object, got: {value}");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inline_json_document() {
        let input = resolve_input(Some(r#"{"a":19,"b":23}"#), &[]).unwrap();
        assert_eq!(input, json!({"a":19,"b":23}));
    }

    #[test]
    fn pairs_only_with_json_value_parsing() {
        let input = resolve_input(None, &["a=19".into(), "name=Platform".into()]).unwrap();
        assert_eq!(input, json!({"a":19,"name":"Platform"}));
    }

    #[test]
    fn pairs_override_document_keys() {
        let input = resolve_input(Some(r#"{"a":1,"b":2}"#), &["b=42".into()]).unwrap();
        assert_eq!(input, json!({"a":1,"b":42}));
    }

    #[test]
    fn at_file_reads_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.json");
        std::fs::write(&path, r#"{"team":"Core"}"#).unwrap();
        let arg = format!("@{}", path.display());
        let input = resolve_input(Some(&arg), &[]).unwrap();
        assert_eq!(input, json!({"team":"Core"}));
    }

    #[test]
    fn non_object_json_is_rejected() {
        let err = resolve_input(Some("[1,2]"), &[]).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }
}
