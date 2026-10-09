//! Plan documents: user-authored YAML plans, exposed to the agent as tools
//! and runnable directly via `graph plan run`.

use super::plan::{check_step_id, Plan};
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exemplars: Vec<String>,
    /// MCP server names whose tools the steps use; the plan tool is hidden
    /// when any is missing from the config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_servers: Vec<String>,
    /// JSON Schema for the plan's inputs (referenced as `{{input.x}}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub steps: Plan,
    #[serde(default)]
    pub finish: Finish,
    /// Source file, set by the loader.
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
    #[error("{path}: duplicate plan identifier '{identifier}' — the earlier file wins")]
    Duplicate { path: String, identifier: String },
    #[error("{}", crate::format::window_message(crate::format::Kind::Plan, path, *found, *oldest, *max))]
    Unsupported {
        path: String,
        found: u32,
        oldest: u32,
        max: u32,
    },
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

impl DocError {
    /// The file (or directory) the error is about.
    pub fn path(&self) -> &str {
        match self {
            DocError::Invalid { path, .. }
            | DocError::Duplicate { path, .. }
            | DocError::Unsupported { path, .. }
            | DocError::Io { path, .. } => path,
        }
    }
}

/// A plan withheld from the catalog because `requires_servers` names MCP
/// servers that aren't configured — not broken, just not runnable here.
#[derive(Debug)]
pub struct HiddenPlan {
    pub identifier: String,
    pub missing_servers: Vec<String>,
}

/// The plan catalog: every document that loaded, plus one error per file
/// that didn't. A broken file never takes the catalog down — callers
/// surface `skipped` as diagnostics.
#[derive(Debug, Default)]
pub struct LoadedPlans {
    pub docs: Vec<PlanDoc>,
    pub skipped: Vec<DocError>,
    /// Plans hidden by unconfigured `requires_servers` (populated by the
    /// CLI runtime, which knows the MCP config). Naming one directly must
    /// fail loudly with the missing servers, never "no plan named".
    pub hidden: Vec<HiddenPlan>,
}

impl LoadedPlans {
    /// Why a plan with this identifier is hidden from the catalog, if it is.
    pub fn hidden_reason(&self, identifier: &str) -> Option<String> {
        let hidden = self.hidden.iter().find(|h| h.identifier == identifier)?;
        Some(format!(
            "plan '{identifier}' requires MCP server(s) {} — configure them \
             under [mcp.<name>] to use it",
            hidden
                .missing_servers
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
    /// Why a plan with this identifier is absent from `docs`, if a skipped
    /// file's stem matches it (plan files are conventionally named after
    /// their identifier).
    pub fn skip_reason(&self, identifier: &str) -> Option<&DocError> {
        self.skipped.iter().find(|error| {
            Path::new(error.path())
                .file_stem()
                .and_then(|stem| stem.to_str())
                == Some(identifier)
        })
    }
}

/// Load every `*.yaml`/`*.yml` under the given directories. Missing
/// directories are skipped; files that fail to read, parse, or validate
/// (and later duplicates of an already-loaded identifier) are reported in
/// `skipped` instead of failing the whole catalog. Files load in sorted
/// order per directory, directories in the order given — so the first
/// definition of an identifier wins deterministically.
pub fn load_plan_docs(dirs: &[PathBuf]) -> LoadedPlans {
    let mut loaded = LoadedPlans::default();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                loaded.skipped.push(DocError::Io {
                    path: dir.display().to_string(),
                    source: e,
                });
                continue;
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                )
            })
            .collect();
        paths.sort();
        for path in paths {
            match load_plan_doc(&path) {
                Ok(doc) if loaded.docs.iter().any(|d| d.identifier == doc.identifier) => {
                    loaded.skipped.push(DocError::Duplicate {
                        path: path.display().to_string(),
                        identifier: doc.identifier,
                    });
                }
                Ok(doc) => loaded.docs.push(doc),
                Err(error) => loaded.skipped.push(error),
            }
        }
    }
    loaded
}

pub const BUILTIN_PLANS: &[&str] = &[
    include_str!("../plans/search_tools.yaml"),
    include_str!("../plans/compose_plan.yaml"),
];

pub fn builtin_plan_docs() -> Vec<PlanDoc> {
    let mut loaded = LoadedPlans::default();
    add_builtin_plans(&mut loaded, BUILTIN_PLANS);
    loaded.docs
}

pub fn add_builtin_plans(loaded: &mut LoadedPlans, sources: &[&str]) {
    for raw in sources {
        let doc = match parse_plan_source(raw, "built-in plan") {
            Ok(doc) => doc,
            Err(error) => {
                loaded.skipped.push(error);
                continue;
            }
        };
        if let Err(message) = validate_doc(&doc) {
            loaded.skipped.push(DocError::Invalid {
                path: format!("built-in plan '{}'", doc.identifier),
                message,
            });
            continue;
        }
        if !loaded.docs.iter().any(|d| d.identifier == doc.identifier) {
            loaded.docs.push(doc);
        }
    }
}

/// Read a plan file for *execution*: parsed and structurally valid.
pub fn load_plan_doc(path: &Path) -> Result<PlanDoc, DocError> {
    let doc = parse_plan_doc(path)?;
    validate_doc(&doc).map_err(|message| DocError::Invalid {
        path: path.display().to_string(),
        message,
    })?;
    Ok(doc)
}

/// Read a plan file for *authoring*: parsed, but not validated.
///
/// [`load_plan_doc`] refuses a document that fails [`validate_doc`], which is
/// right for the runtime catalog and wrong for editing — a plan part-way
/// through being built (a fresh `graph plan new` scaffold has no steps yet)
/// must still be openable, or repairing it would be impossible. That is the
/// same reason the edit guard tolerates pre-existing problems.
///
/// Callers are expected to report the problems themselves rather than
/// pretending the file is sound.
pub fn parse_plan_doc(path: &Path) -> Result<PlanDoc, DocError> {
    let raw = std::fs::read_to_string(path).map_err(|e| DocError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let mut doc = parse_plan_source(&raw, &path.display().to_string())?;
    doc.path = Some(path.to_path_buf());
    Ok(doc)
}

pub fn parse_plan_source(raw: &str, path: &str) -> Result<PlanDoc, DocError> {
    let invalid = |message: String| DocError::Invalid {
        path: path.to_string(),
        message,
    };
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|e| invalid(e.to_string()))?;
    let upgrade =
        crate::format::upgrade(crate::format::Kind::Plan, &mut value).map_err(|e| match e {
            crate::format::FormatError::Unsupported {
                found, oldest, max, ..
            } => DocError::Unsupported {
                path: path.to_string(),
                found,
                oldest,
                max,
            },
            crate::format::FormatError::Invalid(message) => invalid(message),
        })?;
    check_step_keys(&value).map_err(invalid)?;
    check_finish_key(&value).map_err(invalid)?;
    if upgrade.declared.is_none() && !upgrade.migrated() {
        return serde_yaml::from_str(raw).map_err(|e| invalid(e.to_string()));
    }
    serde_yaml::from_value(value).map_err(|e| invalid(e.to_string()))
}

pub const FINISH_FORMS: &str =
    "`finish: silent`, `finish: { output: {…} }` or `finish: { solver: {…} }`";

pub fn check_finish_key(doc: &serde_yaml::Value) -> Result<(), String> {
    let Some(mapping) = doc.as_mapping() else {
        return Ok(());
    };
    if let Some(key) = ["output", "solver"]
        .into_iter()
        .find(|key| mapping.contains_key(*key))
    {
        return Err(format!(
            "`{key}` belongs under `finish` (`finish: {{ {key}: … }}`), not at the top level"
        ));
    }
    if !mapping.contains_key("finish") {
        return Err(format!(
            "the plan has no `finish`; it ends with one of {FINISH_FORMS}"
        ));
    }
    Ok(())
}

const STEP_KEYS: &[&str] = &["id", "tool_name", "toolName", "input", "reasoning"];

/// Reject a key on any step, at any depth, that the step grammar does not
/// know. Files are read strictly; the planner's structured output is not
/// (`Step` itself stays lenient), which is why this lives in the file path.
pub fn check_step_keys(doc: &serde_yaml::Value) -> Result<(), String> {
    let Some(steps) = doc.get("steps") else {
        return Ok(());
    };
    check_body_keys(steps, "steps")
}

fn check_body_keys(body: &serde_yaml::Value, at: &str) -> Result<(), String> {
    match body {
        serde_yaml::Value::Sequence(steps) => steps
            .iter()
            .enumerate()
            .try_for_each(|(index, step)| check_one_step_keys(step, &format!("{at}[{index}]"))),
        serde_yaml::Value::Mapping(_) => check_one_step_keys(body, at),
        _ => Ok(()),
    }
}

fn check_one_step_keys(step: &serde_yaml::Value, at: &str) -> Result<(), String> {
    let Some(mapping) = step.as_mapping() else {
        return Ok(());
    };
    if let Some(unknown) = mapping
        .keys()
        .filter_map(serde_yaml::Value::as_str)
        .find(|key| !STEP_KEYS.contains(key))
    {
        return Err(format!(
            "{at}: unknown field `{unknown}`, expected one of `id`, `tool_name`, `input`, `reasoning`"
        ));
    }
    let tool = step
        .get("tool_name")
        .or_else(|| step.get("toolName"))
        .and_then(serde_yaml::Value::as_str);
    let Some(input) = step.get("input") else {
        return Ok(());
    };
    match tool {
        Some(super::ROUTE_TOOL) => {
            ["then", "else"].iter().try_for_each(|side| {
                input.get(side).map_or(Ok(()), |branch| {
                    check_body_keys(branch, &format!("{at}.input.{side}"))
                })
            })?;
            let Some(cases) = input.get("cases").and_then(serde_yaml::Value::as_mapping) else {
                return Ok(());
            };
            cases.iter().try_for_each(|(key, branch)| {
                let key = key.as_str().unwrap_or("?");
                check_body_keys(branch, &format!("{at}.input.cases.{key}"))
            })
        }
        Some(super::MAP_TOOL | super::REDUCE_TOOL) => input.get("do").map_or(Ok(()), |body| {
            check_body_keys(body, &format!("{at}.input.do"))
        }),
        _ => Ok(()),
    }
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
    let all_ids: Vec<&str> = doc.steps.iter().map(|s| s.id.as_str()).collect();
    let mut seen: Vec<&str> = vec!["input"];
    for step in &doc.steps {
        check_step_id(&step.id)?;
        if seen.contains(&step.id.as_str()) {
            return Err(format!("duplicate step id '{}'", step.id));
        }
        if let Some(problem) = super::plan::workbench_tool_problem(&step.tool_name) {
            return Err(format!("step {}: {problem}", step.id));
        }
        if !step.tool_name.contains("__")
            && step.tool_name != "plan_and_execute"
            && step.tool_name != super::EXIT_TOOL
            && step.tool_name != super::AGENT_TOOL
            && step.tool_name != super::ASK_TOOL
            && step.tool_name != super::ROUTE_TOOL
            && step.tool_name != super::FILTER_TOOL
            && step.tool_name != super::MAP_TOOL
            && step.tool_name != super::REDUCE_TOOL
        {
            return Err(format!(
                "step {} tool '{}' is not a namespaced tool name (like \
                 linear__list_issues) or one of the control steps: \
                 exit, agent, ask, route, filter, map, reduce, plan_and_execute",
                step.id, step.tool_name
            ));
        }
        // Control steps are body-aware: body-internal references
        // (same-body ids, per-item pseudo-roots) are legal, so the generic
        // template walk would false-flag them.
        let mut problems = Vec::new();
        if step.tool_name == super::EXIT_TOOL {
            super::exit::check_exit_input(&step.input, &step.id, &mut problems);
        }
        match step.tool_name.as_str() {
            name if name == super::AGENT_TOOL => {
                super::agent::validate_agent_input(&step.input, &seen, &step.id, &mut problems)
            }
            name if name == super::ASK_TOOL => {
                super::ask::validate_ask_input(&step.input, &seen, &step.id, &mut problems)
            }
            name if name == super::ROUTE_TOOL => super::route::validate_route_input(
                &step.input,
                &seen,
                &all_ids,
                &step.id,
                &mut problems,
            ),
            name if name == super::FILTER_TOOL => {
                super::filter::validate_filter_input(&step.input, &seen, &step.id, &mut problems)
            }
            name if name == super::MAP_TOOL => super::iterate::validate_map_input(
                &step.input,
                &seen,
                &all_ids,
                &step.id,
                &mut problems,
            ),
            name if name == super::REDUCE_TOOL => super::iterate::validate_reduce_input(
                &step.input,
                &seen,
                &all_ids,
                &step.id,
                &mut problems,
            ),
            _ => {
                for value in step.input.values() {
                    check_value_templates(value, &seen, &step.id)?;
                }
            }
        }
        if let Some(problem) = problems.into_iter().next() {
            return Err(problem);
        }
        seen.push(&step.id);
    }
    finish_problems(doc)
}

fn check_finish_templates(map: &Map<String, Value>) -> Result<(), String> {
    for value in map.values() {
        if let Value::String(template) = value {
            crate::template::referenced_roots(template).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn finish_problems(doc: &PlanDoc) -> Result<(), String> {
    match &doc.finish {
        Finish::Solver(solver) => {
            check_finish_templates(&solver.data)?;
            crate::template::referenced_roots(&solver.query_to_answer)
                .map_err(|e| e.to_string())?;
        }
        Finish::Output(output) => check_finish_templates(output)?,
        Finish::Silent => {}
    }
    let schema = match (&doc.output_schema, &doc.finish) {
        (None, _) => None,
        (Some(schema), Finish::Output(output)) => Some(output_schema_problems(schema, output)?),
        (Some(_), _) => {
            return Err(format!(
                "`output_schema` describes an output plan's result, but this plan finishes with \
                 `{}`, whose result is {}",
                finish_label(&doc.finish),
                result_shape(&doc.finish)
            ))
        }
    };
    let steps = serde_json::json!(doc.steps);
    let problems: Vec<String> = exit_inputs(&steps)
        .into_iter()
        .filter_map(|(id, input)| exit_problem(&id, input, &doc.finish, schema.as_ref()))
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

pub fn finish_label(finish: &Finish) -> &'static str {
    match finish {
        Finish::Silent => "silent",
        Finish::Output(_) => "output",
        Finish::Solver(_) => "solver",
    }
}

fn result_shape(finish: &Finish) -> &'static str {
    match finish {
        Finish::Silent => "`{ok, steps_executed}`",
        Finish::Output(_) => "the `finish.output` map",
        Finish::Solver(_) => "`{answer}`",
    }
}

fn is_template(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("{{"),
        Value::Array(items) => items.iter().any(is_template),
        Value::Object(map) => map.values().any(is_template),
        _ => false,
    }
}

fn key_mismatch(expected: &[&str], actual: &Map<String, Value>) -> Option<String> {
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|key| !actual.contains_key(*key))
        .collect();
    let extra: Vec<&str> = actual
        .keys()
        .map(String::as_str)
        .filter(|key| !expected.contains(key))
        .collect();
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("missing {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        parts.push(format!("extra {}", extra.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn literal_problems(
    properties: &Map<String, Value>,
    values: &Map<String, Value>,
    at: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (key, value) in values {
        let Some(property) = properties.get(key) else {
            continue;
        };
        if is_template(value) {
            continue;
        }
        let Ok(validator) = jsonschema::validator_for(property) else {
            continue;
        };
        problems.extend(
            validator
                .iter_errors(value)
                .map(|error| format!("{at}: `{key}` {error}")),
        );
    }
    problems
}

fn output_schema_problems(
    schema: &Value,
    output: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    if !schema.is_object() || jsonschema::validator_for(schema).is_err() {
        return Err("`output_schema` must be a valid JSON Schema object".to_string());
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Err("`output_schema` must list the result's keys under `properties`".to_string());
    };
    let expected: Vec<&str> = properties.keys().map(String::as_str).collect();
    if let Some(mismatch) = key_mismatch(&expected, output) {
        return Err(format!(
            "`finish.output` keys must match `output_schema` properties: {mismatch}"
        ));
    }
    let problems = literal_problems(properties, output, "finish.output");
    if problems.is_empty() {
        Ok(properties.clone())
    } else {
        Err(problems.join("; "))
    }
}

fn exit_problem(
    id: &str,
    input: &Map<String, Value>,
    finish: &Finish,
    schema: Option<&Map<String, Value>>,
) -> Option<String> {
    let status = input.get("status").and_then(Value::as_str);
    let output = input.get("output");
    if status == Some("error") {
        return output.map(|_| {
            format!(
                "step {id}: an error exit cannot carry `output`; a failed run returns the \
                 standard error"
            )
        });
    }
    let Finish::Output(expected) = finish else {
        return output.map(|_| {
            format!(
                "step {id}: exit `output` needs `finish: output`; a {} plan's exits return {}",
                finish_label(finish),
                result_shape(finish)
            )
        });
    };
    if status != Some("success") {
        return None;
    }
    let keys: Vec<&str> = expected.keys().map(String::as_str).collect();
    let Some(Value::Object(output)) = output else {
        return Some(format!(
            "step {id}: a success exit in an output plan returns the plan's result; give it \
             `output` with the `finish.output` keys ({})",
            keys.join(", ")
        ));
    };
    if let Some(mismatch) = key_mismatch(&keys, output) {
        return Some(format!(
            "step {id}: exit `output` keys must match `finish.output`: {mismatch}"
        ));
    }
    let problems = schema
        .map(|schema| literal_problems(schema, output, &format!("step {id} output")))
        .unwrap_or_default();
    (!problems.is_empty()).then(|| problems.join("; "))
}

fn exit_inputs(steps: &Value) -> Vec<(String, &Map<String, Value>)> {
    let mut found = Vec::new();
    collect_exits(steps, &mut found);
    found
}

fn collect_exits<'a>(body: &'a Value, found: &mut Vec<(String, &'a Map<String, Value>)>) {
    match body {
        Value::Array(steps) => steps.iter().for_each(|step| collect_exits(step, found)),
        Value::Object(step) => {
            let tool = step
                .get("tool_name")
                .or_else(|| step.get("toolName"))
                .and_then(Value::as_str);
            let Some(Value::Object(input)) = step.get("input") else {
                return;
            };
            match tool {
                Some(super::EXIT_TOOL) => {
                    let id = step.get("id").and_then(Value::as_str).unwrap_or("exit");
                    found.push((id.to_string(), input));
                }
                Some(super::ROUTE_TOOL) => {
                    for side in ["then", "else"] {
                        if let Some(branch) = input.get(side) {
                            collect_exits(branch, found);
                        }
                    }
                    if let Some(Value::Object(cases)) = input.get("cases") {
                        cases
                            .values()
                            .for_each(|branch| collect_exits(branch, found));
                    }
                }
                Some(super::MAP_TOOL | super::REDUCE_TOOL) => {
                    if let Some(body) = input.get("do") {
                        collect_exits(body, found);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn check_value_templates(value: &Value, available: &[&str], step_id: &str) -> Result<(), String> {
    match value {
        Value::String(s) if s.contains("{{") => {
            let roots =
                crate::template::referenced_roots(s).map_err(|e| format!("step {step_id}: {e}"))?;
            for root in roots {
                if !available.contains(&root.as_str()) {
                    return Err(format!(
                        "step {step_id} references {root}, which is not `input` \
                         or an earlier step"
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

impl std::str::FromStr for PlanDoc {
    type Err = DocError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        parse_plan_source(raw, "plan")
    }
}

impl PlanDoc {
    pub fn solver(&self) -> Option<&super::SolverData> {
        match &self.finish {
            Finish::Solver(solver) => Some(solver),
            _ => None,
        }
    }

    pub fn output(&self) -> Option<&Map<String, Value>> {
        match &self.finish {
            Finish::Output(output) => Some(output),
            _ => None,
        }
    }

    pub fn result_schema(&self) -> Value {
        match &self.finish {
            Finish::Output(output) => self.output_schema.clone().unwrap_or_else(|| {
                let properties: Map<String, Value> = output
                    .keys()
                    .map(|key| (key.clone(), Value::Object(Map::new())))
                    .collect();
                serde_json::json!({
                    "type": "object",
                    "required": output.keys().collect::<Vec<_>>(),
                    "properties": properties,
                })
            }),
            Finish::Solver(_) => serde_json::json!({
                "type": "object",
                "required": ["answer"],
                "properties": {"answer": {"type": "string"}},
            }),
            Finish::Silent => serde_json::json!({
                "type": "object",
                "required": ["ok", "steps_executed"],
                "properties": {
                    "ok": {"type": "boolean"},
                    "steps_executed": {"type": "integer"},
                },
            }),
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
        let loaded = load_plan_docs(&[dir.path().to_path_buf()]);
        assert_eq!(loaded.docs.len(), 1);
        assert!(loaded.skipped.is_empty());
        let doc = &loaded.docs[0];
        assert_eq!(doc.identifier, "sprint_analysis");
        assert_eq!(doc.steps[1].tool_name, "linear__list_issues");
        assert!(doc.tool_description().contains("how is my sprint going"));
    }

    #[test]
    fn a_stray_step_key_is_a_load_error_at_any_depth() {
        let top = "identifier: p\nname: P\ndescription: d\nsteps:\n  - id: E0\n    tool_name: t__x\n    input: {}\n    future_field: 1\n";
        let err = parse_plan_source(top, "p.yaml").unwrap_err().to_string();
        assert!(
            err.contains("steps[0]: unknown field `future_field`"),
            "{err}"
        );

        let nested = "identifier: p\nname: P\ndescription: d\nsteps:\n  - id: E0\n    tool_name: route\n    input:\n      if: { value: x, op: eq, to: x }\n      then:\n        - id: E1\n          tool_name: map\n          input:\n            over: \"{{input.items}}\"\n            do:\n              tool_name: t__y\n              input: {}\n              retries: 3\n";
        let err = parse_plan_source(nested, "p.yaml").unwrap_err().to_string();
        assert!(
            err.contains("steps[0].input.then[0].input.do: unknown field `retries`"),
            "{err}"
        );

        let ordinary_tool_input = "identifier: p\nname: P\ndescription: d\nsteps:\n  - id: E0\n    tool_name: t__x\n    input: { do: { then: 1 }, then: 2 }\n";
        assert!(parse_plan_source(ordinary_tool_input, "p.yaml").is_ok());

        let camel: crate::pipeline::Step = serde_json::from_value(
            serde_json::json!({"id": "E0", "toolName": "t__x", "input": {}, "extra": true}),
        )
        .unwrap();
        assert_eq!(camel.tool_name, "t__x");
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

    fn doc_from(yaml: &str) -> Result<PlanDoc, String> {
        let doc: PlanDoc = yaml.parse().map_err(|e: DocError| e.to_string())?;
        validate_doc(&doc).map(|()| doc)
    }

    /// Plan documents are the human-authored surface, so every control
    /// step's input validator has to run here — not only on planner-written
    /// plans. An agent step whose schema, budget, or prompt is malformed
    /// must fail at load, before it can fail mid-run (where a planned run
    /// would spend a replan attempt "fixing" a static defect).
    #[test]
    fn agent_step_input_is_validated_in_plan_documents() {
        fn doc_with(input_block: &str) -> Result<PlanDoc, String> {
            let head = "identifier: probe\nname: Probe\ndescription: d\n\
                        steps:\n  - id: E0\n    tool_name: agent\n    input:\n";
            let tail = "output:\n  x: \"{{E0.output}}\"\n";
            doc_from(&(head.to_string() + input_block + tail))
        }

        let cases = [
            (
                "      prompt: 12345\n      outputSchema: { type: object }\n",
                "invalid agent input",
            ),
            (
                "      prompt: hi\n      maxIterations: 0\n      outputSchema: { type: object }\n",
                "at least 1",
            ),
            (
                "      prompt: hi\n      outputSchema: { type: string }\n",
                "type \"object\"",
            ),
            (
                "      prompt: hi\n      outputSchema: { type: object }\n      unknownField: nope\n",
                "invalid agent input",
            ),
            (
                "      prompt: hi\n      outputSchema: { type: object }\n      tools: []\n",
                "`tools` is empty",
            ),
            (
                "      prompt: \"{{E7.values}}\"\n      outputSchema: { type: object }\n",
                "E7",
            ),
        ];
        for (input, expected) in cases {
            let Err(err) = doc_with(input) else {
                panic!("expected a validation error for:\n{input}");
            };
            assert!(err.contains(expected), "for\n{input}got: {err}");
        }

        // The well-formed shape still loads.
        doc_with("      prompt: hi\n      outputSchema: { type: object, properties: {} }\n")
            .expect("a valid agent step must load");
    }

    #[test]
    fn accepts_descriptive_step_ids() {
        let doc = doc_from(&DOC.replace("E0", "find_team").replace("E1", "team_issues")).unwrap();
        assert_eq!(doc.steps[1].id, "team_issues");
    }

    #[test]
    fn rejects_duplicate_reserved_and_malformed_ids() {
        let dup = DOC.replace("id: E1", "id: E0");
        assert!(doc_from(&dup).unwrap_err().contains("duplicate step id"));

        // Reserved template roots can't be shadowed — E1 references would
        // dangle, so rename them too.
        let reserved = DOC.replace("E1", "item");
        assert!(doc_from(&reserved).unwrap_err().contains("reserved"));

        let malformed = DOC.replace("id: E1", "id: 2fast");
        assert!(doc_from(&malformed)
            .unwrap_err()
            .contains("must be an identifier"));
    }

    #[test]
    fn unknown_bare_tool_name_error_lists_the_control_steps() {
        let err = doc_from(&DOC.replace("linear__list_issues", "gate")).unwrap_err();
        assert!(err.contains("'gate'"), "{err}");
        assert!(
            err.contains("exit, agent, ask, route, filter, map, reduce, plan_and_execute"),
            "{err}"
        );
    }

    #[test]
    fn rejects_workbench_tools_outright() {
        let err = doc_from(&DOC.replace("linear__list_issues", "workbench__grep")).unwrap_err();
        assert!(err.contains("'workbench__grep'"), "{err}");
        assert!(err.contains("not available in the plan runtime"), "{err}");
    }

    #[test]
    fn rejects_references_to_unknown_roots() {
        let typo = DOC.replace("{{E0.values.0.id}}", "{{find_team.values.0.id}}");
        let err = doc_from(&typo).unwrap_err();
        assert!(err.contains("find_team"), "{err}");
        assert!(err.contains("not `input` or an earlier step"), "{err}");
    }

    #[test]
    fn section_scoped_bare_keys_are_not_root_references() {
        // Bare keys inside {{#E0.values}}…{{/E0.values}} read the current
        // item, so they must not be flagged as unknown roots — while a
        // dotted path with a typo'd root inside the section still is.
        let sectioned = DOC.replace(
            r#"{ teamId: "{{E0.values.0.id}}" }"#,
            r#"{ teamId: "{{#E0.values}}{{id}} {{#name}}{{name}}{{/name}}{{/E0.values}}" }"#,
        );
        doc_from(&sectioned).unwrap();

        let typo = DOC.replace(
            r#"{ teamId: "{{E0.values.0.id}}" }"#,
            r#"{ teamId: "{{#E0.values}}{{E9.summary}}{{/E0.values}}" }"#,
        );
        let err = doc_from(&typo).unwrap_err();
        assert!(err.contains("E9"), "{err}");
    }

    #[test]
    fn later_duplicate_identifiers_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), DOC).unwrap();
        std::fs::write(dir.path().join("b.yaml"), DOC).unwrap();
        let loaded = load_plan_docs(&[dir.path().to_path_buf()]);
        // Sorted order: a.yaml wins, b.yaml is reported.
        assert_eq!(loaded.docs.len(), 1);
        assert_eq!(
            loaded.docs[0].path.as_deref(),
            Some(dir.path().join("a.yaml").as_path())
        );
        assert!(
            matches!(&loaded.skipped[..], [DocError::Duplicate { path, .. }] if path.ends_with("b.yaml"))
        );
    }

    #[test]
    fn a_plan_file_replaces_a_builtin_plan_with_the_same_identifier() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mine.yaml"), DOC).unwrap();
        let mut loaded = load_plan_docs(&[dir.path().to_path_buf()]);
        let builtin_same = DOC.replace("Sprint Analysis", "Built-in Sprint");
        let builtin_other = DOC.replace("sprint_analysis", "builtin_only");
        add_builtin_plans(&mut loaded, &[&builtin_same, &builtin_other]);
        let ids: Vec<&str> = loaded.docs.iter().map(|d| d.identifier.as_str()).collect();
        assert_eq!(ids, ["sprint_analysis", "builtin_only"]);
        assert_eq!(loaded.docs[0].name, "Sprint Analysis", "the file wins");
        assert!(loaded.docs[0].path.is_some());
        assert!(loaded.docs[1].path.is_none(), "a built-in has no path");
        assert!(loaded.skipped.is_empty(), "an override is not a duplicate");
    }

    #[test]
    fn an_invalid_builtin_plan_is_skipped() {
        let mut loaded = LoadedPlans::default();
        add_builtin_plans(&mut loaded, &["steps: {"]);
        assert!(loaded.docs.is_empty());
        assert_eq!(loaded.skipped.len(), 1);
    }

    #[test]
    fn broken_files_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.yaml"), DOC).unwrap();
        std::fs::write(dir.path().join("bad.yaml"), "steps: {").unwrap();
        std::fs::write(
            dir.path().join("invalid.yaml"),
            DOC.replace("{{E0.values.0.id}}", "{{E5.values.0.id}}")
                .replace("sprint_analysis", "forward_ref"),
        )
        .unwrap();
        let loaded = load_plan_docs(&[dir.path().to_path_buf()]);
        assert_eq!(loaded.docs.len(), 1);
        assert_eq!(loaded.docs[0].identifier, "sprint_analysis");
        assert_eq!(loaded.skipped.len(), 2);
        // skip_reason matches a skipped file's stem to the identifier the
        // caller asked for.
        let reason = loaded.skip_reason("invalid").unwrap();
        assert!(reason.to_string().contains("E5"), "{reason}");
        assert!(loaded.skip_reason("sprint_analysis").is_none());
    }

    fn finish_problem(yaml: &str) -> String {
        let doc = parse_plan_source(yaml, "p.yaml").unwrap();
        validate_doc(&doc).unwrap_err()
    }

    const OUTPUT_HEAD: &str = "version: 2\nidentifier: p\nname: P\ndescription: d\nsteps:\n  - id: E0\n    tool_name: t__x\n    input: {}\n";

    #[test]
    fn a_version_2_plan_states_its_finish() {
        let missing = parse_plan_source(OUTPUT_HEAD, "p.yaml")
            .unwrap_err()
            .to_string();
        assert!(missing.contains("has no `finish`"), "{missing}");
        assert!(missing.contains("finish: silent"), "{missing}");

        let top_level = format!("{OUTPUT_HEAD}output: {{ x: \"{{{{E0}}}}\" }}\n");
        let err = parse_plan_source(&top_level, "p.yaml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`output` belongs under `finish`"), "{err}");

        let bad = format!("{OUTPUT_HEAD}finish: output\n");
        let err = parse_plan_source(&bad, "p.yaml").unwrap_err().to_string();
        assert!(err.contains("finish is one of"), "{err}");

        for (finish, label) in [
            ("finish: silent\n", "silent"),
            ("finish:\n  output: { x: \"{{E0}}\" }\n", "output"),
            ("finish:\n  solver: { query_to_answer: q }\n", "solver"),
        ] {
            let doc = parse_plan_source(&format!("{OUTPUT_HEAD}{finish}"), "p.yaml").unwrap();
            assert_eq!(finish_label(&doc.finish), label);
            validate_doc(&doc).unwrap();
        }
    }

    #[test]
    fn a_version_1_plan_gets_an_explicit_finish() {
        let v1 = "identifier: p\nname: P\ndescription: d\nsteps:\n  - id: E0\n    tool_name: t__x\n    input: {}\n";
        let silent = parse_plan_source(v1, "p.yaml").unwrap();
        assert!(matches!(silent.finish, Finish::Silent));
        let output =
            parse_plan_source(&format!("{v1}output: {{ x: \"{{{{E0}}}}\" }}\n"), "p.yaml").unwrap();
        assert_eq!(output.output().unwrap()["x"], "{{E0}}");
        let solver =
            parse_plan_source(&format!("{v1}solver: {{ query_to_answer: q }}\n"), "p.yaml")
                .unwrap();
        assert_eq!(solver.solver().unwrap().query_to_answer, "q");
    }

    #[test]
    fn success_exits_in_an_output_plan_return_the_finish_keys() {
        let plan = |exit_output: &str| {
            format!(
                "{OUTPUT_HEAD}  - id: stop\n    tool_name: exit\n    input:\n      status: success\n{exit_output}finish:\n  output: {{ count: \"{{{{E0.n}}}}\", items: \"{{{{E0.items}}}}\" }}\n"
            )
        };
        let missing = finish_problem(&plan("      output: { count: 0 }\n"));
        assert!(
            missing.contains(
                "step stop: exit `output` keys must match `finish.output`: missing items"
            ),
            "{missing}"
        );
        let extra = finish_problem(&plan("      output: { count: 0, items: [], more: 1 }\n"));
        assert!(extra.contains("extra more"), "{extra}");
        let absent = finish_problem(&plan(""));
        assert!(
            absent.contains("give it `output` with the `finish.output` keys (count, items)"),
            "{absent}"
        );
        let doc =
            parse_plan_source(&plan("      output: { count: 0, items: [] }\n"), "p.yaml").unwrap();
        validate_doc(&doc).unwrap();
    }

    #[test]
    fn only_success_exits_in_output_plans_carry_output() {
        let error_exit = format!(
            "{OUTPUT_HEAD}  - id: stop\n    tool_name: exit\n    input:\n      status: error\n      output: {{ x: 1 }}\nfinish:\n  output: {{ x: \"{{{{E0}}}}\" }}\n"
        );
        assert!(finish_problem(&error_exit).contains("an error exit cannot carry `output`"));
        for finish in [
            "finish: silent\n",
            "finish:\n  solver: { query_to_answer: q }\n",
        ] {
            let doc = format!(
                "{OUTPUT_HEAD}  - id: stop\n    tool_name: exit\n    input:\n      status: success\n      output: {{ x: 1 }}\n{finish}"
            );
            assert!(finish_problem(&doc).contains("exit `output` needs `finish: output`"));
        }
        let nested = format!(
            "{OUTPUT_HEAD}  - id: E1\n    tool_name: route\n    input:\n      if: {{ value: 1, op: eq, to: 1 }}\n      then:\n        - id: B0\n          tool_name: exit\n          input: {{ status: success }}\nfinish:\n  output: {{ x: \"{{{{E0}}}}\" }}\n"
        );
        assert!(finish_problem(&nested).contains("step B0"));
    }

    #[test]
    fn an_output_schema_names_the_finish_keys_and_checks_literals() {
        let plan = |schema: &str, exit: &str| {
            format!(
                "version: 2\nidentifier: p\nname: P\ndescription: d\noutput_schema: {schema}\nsteps:\n  - id: E0\n    tool_name: t__x\n    input: {{}}\n  - id: stop\n    tool_name: exit\n    input:\n      status: success\n      output: {exit}\nfinish:\n  output: {{ count: \"{{{{E0.n}}}}\" }}\n"
            )
        };
        let schema = "{ type: object, properties: { count: { type: integer } } }";
        let doc = parse_plan_source(&plan(schema, "{ count: 0 }"), "p.yaml").unwrap();
        validate_doc(&doc).unwrap();
        assert_eq!(
            doc.result_schema()["properties"]["count"]["type"],
            "integer"
        );

        let wrong_type = finish_problem(&plan(schema, "{ count: none }"));
        assert!(
            wrong_type.contains("step stop output: `count`"),
            "{wrong_type}"
        );
        let other_keys = finish_problem(&plan(
            "{ type: object, properties: { total: {} } }",
            "{ count: 0 }",
        ));
        assert!(
            other_keys.contains("`finish.output` keys must match `output_schema` properties"),
            "{other_keys}"
        );

        let solver = "version: 2\nidentifier: p\nname: P\ndescription: d\noutput_schema: { type: object, properties: {} }\nsteps:\n  - id: E0\n    tool_name: t__x\n    input: {}\nfinish:\n  solver: { query_to_answer: q }\n";
        assert!(
            finish_problem(solver).contains("`output_schema` describes an output plan's result")
        );
    }

    #[test]
    fn the_result_schema_follows_the_finish() {
        let doc =
            |finish: &str| parse_plan_source(&format!("{OUTPUT_HEAD}{finish}"), "p.yaml").unwrap();
        assert_eq!(
            doc("finish:\n  output: { a: \"{{E0}}\", b: 1 }\n").result_schema(),
            serde_json::json!({"type": "object", "required": ["a", "b"], "properties": {"a": {}, "b": {}}})
        );
        assert_eq!(
            doc("finish:\n  solver: { query_to_answer: q }\n").result_schema()["required"],
            serde_json::json!(["answer"])
        );
        assert_eq!(
            doc("finish: silent\n").result_schema()["required"],
            serde_json::json!(["ok", "steps_executed"])
        );
    }
}
