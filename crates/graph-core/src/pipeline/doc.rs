//! Plan documents: user-authored YAML plans, exposed to the agent as tools
//! and runnable directly via `graph plan run`.

use super::plan::{step_number, Plan, SolverData};
use super::Finish;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDoc {
    /// Tool-name-safe identifier, e.g. `sprint_analysis`.
    pub identifier: String,
    pub name: String,
    pub description: String,
    /// Example queries this plan handles (folded into the tool description).
    #[serde(default)]
    pub exemplars: Vec<String>,
    /// MCP server names whose tools the steps use; the plan tool is hidden
    /// when any is missing from the config.
    #[serde(default)]
    pub requires_servers: Vec<String>,
    /// JSON Schema for the plan's inputs (referenced as `{{input.x}}`).
    #[serde(default)]
    pub input_schema: Option<Value>,
    pub steps: Plan,
    /// LLM synthesis of the results into prose. Optional — see `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solver: Option<SolverData>,
    /// Structured output: a template map rendered against results and
    /// emitted as JSON, with no LLM involved. Mutually exclusive with
    /// `solver`; when both are absent the plan is a silent side-effect
    /// plan (runs, exits, no output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Map<String, Value>>,
    /// Source file, set by the loader.
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
    #[error("duplicate plan identifier '{0}'")]
    Duplicate(String),
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// Load every `*.yaml`/`*.yml` under the given directories. Missing
/// directories are skipped; malformed documents are errors.
pub fn load_plan_docs(dirs: &[PathBuf]) -> Result<Vec<PlanDoc>, DocError> {
    let mut docs: Vec<PlanDoc> = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| DocError::Io {
                path: dir.display().to_string(),
                source: e,
            })?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                )
            })
            .collect();
        entries.sort();
        for path in entries {
            let doc = load_plan_doc(&path)?;
            if docs.iter().any(|d| d.identifier == doc.identifier) {
                return Err(DocError::Duplicate(doc.identifier));
            }
            docs.push(doc);
        }
    }
    Ok(docs)
}

pub fn load_plan_doc(path: &Path) -> Result<PlanDoc, DocError> {
    let raw = std::fs::read_to_string(path).map_err(|e| DocError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let mut doc: PlanDoc = serde_yaml::from_str(&raw).map_err(|e| DocError::Invalid {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    doc.path = Some(path.to_path_buf());
    validate_doc(&doc).map_err(|message| DocError::Invalid {
        path: path.display().to_string(),
        message,
    })?;
    Ok(doc)
}

/// Structural validation: identifier shape, step ids, template syntax,
/// reference ordering.
pub fn validate_doc(doc: &PlanDoc) -> Result<(), String> {
    if doc.identifier.is_empty()
        || !doc
            .identifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "identifier '{}' must be non-empty and use only [a-zA-Z0-9_-]",
            doc.identifier
        ));
    }
    if doc.steps.is_empty() {
        return Err("plan has no steps".to_string());
    }
    let mut seen: Vec<&str> = vec!["input"];
    for step in &doc.steps {
        if step_number(&step.id).is_none() {
            return Err(format!("step id '{}' must look like E0, E1, …", step.id));
        }
        if !step.tool_name.contains("__")
            && step.tool_name != "plan_and_execute"
            && step.tool_name != super::EXIT_TOOL
        {
            return Err(format!(
                "step {} tool '{}' is not a namespaced tool name",
                step.id, step.tool_name
            ));
        }
        for value in step.input.values() {
            check_value_templates(value, &seen, &step.id)?;
        }
        seen.push(&step.id);
    }
    if doc.solver.is_some() && doc.output.is_some() {
        return Err("`solver` and `output` are mutually exclusive — pick one".to_string());
    }
    if let Some(solver) = &doc.solver {
        for value in solver.data.values() {
            if let Value::String(template) = value {
                crate::template::referenced_roots(template).map_err(|e| e.to_string())?;
            }
        }
        crate::template::referenced_roots(&solver.query_to_answer).map_err(|e| e.to_string())?;
    }
    if let Some(output) = &doc.output {
        for value in output.values() {
            if let Value::String(template) = value {
                crate::template::referenced_roots(template).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

fn check_value_templates(value: &Value, available: &[&str], step_id: &str) -> Result<(), String> {
    match value {
        Value::String(s) if s.contains("{{") => {
            let roots =
                crate::template::referenced_roots(s).map_err(|e| format!("step {step_id}: {e}"))?;
            for root in roots {
                if step_number(&root).is_some() && !available.contains(&root.as_str()) {
                    return Err(format!(
                        "step {step_id} references {root}, which is not an earlier step"
                    ));
                }
            }
            Ok(())
        }
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_value_templates(item, available, step_id)),
        Value::Object(map) => map
            .values()
            .try_for_each(|child| check_value_templates(child, available, step_id)),
        _ => Ok(()),
    }
}

/// Fill in top-level `default` values from a JSON Schema's properties for
/// any keys absent from the input object.
pub fn apply_schema_defaults(schema: &Value, input: &mut Value) {
    let (Some(properties), Some(map)) = (
        schema.get("properties").and_then(Value::as_object),
        input.as_object_mut(),
    ) else {
        return;
    };
    for (key, prop) in properties {
        if let Some(default) = prop.get("default") {
            map.entry(key.clone()).or_insert_with(|| default.clone());
        }
    }
}

/// Validate plan inputs against the doc's schema; Err carries one message
/// per problem (missing required field, wrong type, …).
pub fn validate_input(doc: &PlanDoc, input: &Value) -> Result<(), Vec<String>> {
    let Some(schema) = &doc.input_schema else {
        return Ok(());
    };
    let Ok(validator) = jsonschema::validator_for(schema) else {
        return Err(vec![format!(
            "plan '{}' has an invalid input_schema",
            doc.identifier
        )]);
    };
    let problems: Vec<String> = validator
        .iter_errors(input)
        .map(|e| e.to_string())
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

impl PlanDoc {
    /// How the plan finishes: solve, render structured output, or silence.
    pub fn finish(&self) -> Finish {
        if let Some(solver) = &self.solver {
            Finish::Solve(solver.clone())
        } else if let Some(output) = &self.output {
            Finish::Render(output.clone())
        } else {
            Finish::Silent
        }
    }

    /// The tool description shown to the agent and planner.
    pub fn tool_description(&self) -> String {
        let mut description = format!("{} — {}", self.name, self.description);
        if !self.exemplars.is_empty() {
            description.push_str("\nExample queries this handles: ");
            description.push_str(&self.exemplars.join("; "));
        }
        description
    }

    /// The input schema exposed on the plan tool.
    pub fn tool_input_schema(&self) -> Value {
        self.input_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
identifier: sprint_analysis
name: Sprint Analysis
description: Analyze the current sprint for a team
exemplars:
  - how is my sprint going
requires_servers: [linear]
input_schema:
  type: object
  required: [team]
  properties:
    team: { type: string }
steps:
  - id: E0
    tool_name: linear__search_teams
    input: { query: "{{input.team}}" }
  - id: E1
    tool_name: linear__list_issues
    input: { teamId: "{{E0.values.0.id}}" }
solver:
  query_to_answer: |
    Summarize the sprint for {{input.team}}.
  data:
    issues: "{{E1}}"
"#;

    #[test]
    fn parses_and_validates_a_doc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sprint.yaml");
        std::fs::write(&path, DOC).unwrap();
        let docs = load_plan_docs(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert_eq!(doc.identifier, "sprint_analysis");
        assert_eq!(doc.steps[1].tool_name, "linear__list_issues");
        assert!(doc.tool_description().contains("how is my sprint going"));
    }

    #[test]
    fn rejects_forward_references_and_bad_ids() {
        let bad = DOC.replace("{{E0.values.0.id}}", "{{E5.values.0.id}}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, bad).unwrap();
        let err = load_plan_doc(&path).unwrap_err();
        assert!(err.to_string().contains("E5"));
    }

    #[test]
    fn duplicate_identifiers_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), DOC).unwrap();
        std::fs::write(dir.path().join("b.yaml"), DOC).unwrap();
        let err = load_plan_docs(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(matches!(err, DocError::Duplicate(_)));
    }
}
