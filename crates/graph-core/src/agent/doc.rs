use crate::template::{referenced_roots, render_str, Roots};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const BUILTINS: &[&str] = &[include_str!("../agents/chat.yaml")];

pub const CHAT_AGENT: &str = "chat";

pub const RESERVED_TOOL_NAMES: &[&str] = &[
    "plan_and_execute",
    "exit",
    "decide",
    "filter",
    "map",
    "reduce",
    "agent",
    "ask",
];

pub const GENERATED_TOOL_PREFIXES: &[&str] = &["agent__", "transfer_to_"];

pub const TEMPLATE_ROOTS: &[&str] = &["rules", "session"];

pub const SESSION_KEYS: &[&str] = &["date", "user"];

const AGENT_KEYS: &[&str] = &[
    "name",
    "description",
    "model",
    "tools",
    "subagents",
    "handoffs",
    "input_schema",
    "output_schema",
    "max_iterations",
    "system_prompt",
];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentSource {
    #[default]
    Builtin,
    File(PathBuf),
}

impl AgentSource {
    pub fn describe(&self) -> String {
        match self {
            AgentSource::Builtin => "built-in".to_string(),
            AgentSource::File(path) => path.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDoc {
    pub name: String,
    pub description: String,
    pub model: String,
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoffs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    pub system_prompt: String,
    #[serde(skip)]
    pub source: AgentSource,
}

impl AgentDoc {
    pub fn subagent_input_schema(&self) -> Value {
        self.input_schema.clone().unwrap_or_else(|| {
            json!({
                "type": "object",
                "required": ["prompt"],
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The task for this agent, stated so it stands on its own"
                    }
                }
            })
        })
    }

    pub fn subagent_output_schema(&self) -> Value {
        self.output_schema.clone().unwrap_or_else(|| {
            json!({
                "type": "object",
                "required": ["result"],
                "properties": {
                    "result": {
                        "type": "string",
                        "description": "The agent's final reply"
                    }
                }
            })
        })
    }
}

pub fn unknown_agent_keys(value: &serde_yaml::Value) -> Vec<String> {
    let Some(mapping) = value.as_mapping() else {
        return Vec::new();
    };
    mapping
        .keys()
        .filter_map(serde_yaml::Value::as_str)
        .filter(|key| !AGENT_KEYS.contains(key))
        .map(str::to_string)
        .collect()
}

pub fn parse_agent_source(raw: &str) -> Result<AgentDoc, String> {
    let mut value: serde_yaml::Value = serde_yaml::from_str(raw).map_err(|e| e.to_string())?;
    if !value.is_mapping() {
        return Err("an agent file must be a YAML mapping".to_string());
    }
    crate::format::upgrade(crate::format::Kind::Agent, &mut value).map_err(|e| e.to_string())?;
    let unknown = unknown_agent_keys(&value);
    if !unknown.is_empty() {
        return Err(format!(
            "unknown field(s) {}",
            unknown
                .iter()
                .map(|key| format!("`{key}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    serde_yaml::from_value(value).map_err(|e| e.to_string())
}

pub fn load_agent_file(path: &Path) -> Result<AgentDoc, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut doc = parse_agent_source(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    doc.source = AgentSource::File(path.to_path_buf());
    Ok(doc)
}

#[derive(Debug, Default)]
pub struct LoadedAgents {
    pub docs: Vec<AgentDoc>,
    pub errors: Vec<String>,
}

pub fn load_agent_dir(dir: &Path) -> LoadedAgents {
    let mut loaded = LoadedAgents::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return loaded;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .collect();
    paths.sort();
    for path in paths {
        match load_agent_file(&path) {
            Ok(doc) => {
                if let Some(first) = loaded.docs.iter().find(|d| d.name == doc.name) {
                    loaded.errors.push(format!(
                        "{}: duplicate agent name '{}', already defined by {}",
                        path.display(),
                        doc.name,
                        first.source.describe()
                    ));
                } else {
                    loaded.docs.push(doc);
                }
            }
            Err(error) => loaded.errors.push(error),
        }
    }
    loaded
}

pub fn agent_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(parent) = graph_config::project_config_path().parent() {
        dirs.push(parent.join("agents"));
    }
    if let Some(parent) = graph_config::global_config_path().parent() {
        dirs.push(parent.join("agents"));
    }
    dirs
}

pub fn global_fragments() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("control_steps", crate::pipeline::CONTROL_STEP_RULES),
        ("templating", crate::pipeline::TEMPLATING_RULES),
    ])
}

pub fn builtin_agents(sources: &[&str]) -> Vec<AgentDoc> {
    sources
        .iter()
        .map(|raw| parse_agent_source(raw).expect("built-in agent files parse"))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct AgentSet {
    agents: BTreeMap<String, AgentDoc>,
}

impl AgentSet {
    pub fn layered(builtins: Vec<AgentDoc>, overrides_lowest_first: Vec<Vec<AgentDoc>>) -> Self {
        let mut agents = BTreeMap::new();
        for doc in builtins
            .into_iter()
            .chain(overrides_lowest_first.into_iter().flatten())
        {
            agents.insert(doc.name.clone(), doc);
        }
        Self { agents }
    }

    pub fn load(builtin_sources: &[&str], dirs_highest_first: &[PathBuf]) -> (Self, Vec<String>) {
        let mut errors = Vec::new();
        let mut layers = Vec::new();
        for dir in dirs_highest_first.iter().rev() {
            let loaded = load_agent_dir(dir);
            errors.extend(loaded.errors);
            layers.push(loaded.docs);
        }
        (
            Self::layered(builtin_agents(builtin_sources), layers),
            errors,
        )
    }

    pub fn builtin() -> Self {
        Self::layered(builtin_agents(BUILTINS), Vec::new())
    }

    pub fn get(&self, name: &str) -> Option<&AgentDoc> {
        self.agents.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &AgentDoc> {
        self.agents.values()
    }

    pub fn validate(&self, fragments: &[&str]) -> Vec<String> {
        let mut problems: Vec<String> = self
            .agents
            .values()
            .flat_map(|doc| self.own_problems(doc, fragments))
            .collect();
        problems.extend(
            self.subagent_cycles()
                .iter()
                .map(|cycle| cycle_problem(cycle)),
        );
        problems
    }

    pub fn agent_problems(&self, name: &str, fragments: &[&str]) -> Vec<String> {
        let Some(doc) = self.agents.get(name) else {
            return vec![format!("agent '{name}' is not defined")];
        };
        let mut problems = self.own_problems(doc, fragments);
        problems.extend(
            self.subagent_cycles()
                .iter()
                .filter(|cycle| cycle.iter().any(|member| member == name))
                .map(|cycle| cycle_problem(cycle)),
        );
        problems
    }

    fn own_problems(&self, doc: &AgentDoc, fragments: &[&str]) -> Vec<String> {
        let mut problems = validate_doc(doc, fragments);
        for target in &doc.subagents {
            if !self.agents.contains_key(target) {
                problems.push(format!("subagent '{target}' is not a defined agent"));
            }
        }
        for target in &doc.handoffs {
            if target == &doc.name {
                problems.push("an agent can't hand off to itself".to_string());
            } else if !self.agents.contains_key(target) {
                problems.push(format!("handoff '{target}' is not a defined agent"));
            }
        }
        problems
            .into_iter()
            .map(|problem| {
                format!(
                    "agent '{}' ({}): {problem}",
                    doc.name,
                    doc.source.describe()
                )
            })
            .collect()
    }

    fn subagent_cycles(&self) -> Vec<Vec<String>> {
        let mut cycles = Vec::new();
        let mut reported: BTreeSet<Vec<String>> = BTreeSet::new();
        for start in self.agents.keys() {
            let mut stack = vec![start.clone()];
            self.walk(start, &mut stack, &mut cycles, &mut reported);
        }
        cycles
    }

    fn walk(
        &self,
        current: &str,
        stack: &mut Vec<String>,
        cycles: &mut Vec<Vec<String>>,
        reported: &mut BTreeSet<Vec<String>>,
    ) {
        let Some(doc) = self.agents.get(current) else {
            return;
        };
        for next in &doc.subagents {
            if next == current {
                continue;
            }
            if let Some(position) = stack.iter().position(|seen| seen == next) {
                let mut cycle: Vec<String> = stack[position..].to_vec();
                let mut key = cycle.clone();
                key.sort();
                if reported.insert(key) {
                    cycle.push(next.clone());
                    cycles.push(cycle);
                }
                continue;
            }
            stack.push(next.clone());
            self.walk(next, stack, cycles, reported);
            stack.pop();
        }
    }
}

fn cycle_problem(cycle: &[String]) -> String {
    format!(
        "subagent cycle: {} (only self-spawn may repeat an agent)",
        cycle.join(" → ")
    )
}

fn template_scope(rules: Map<String, Value>, session: Map<String, Value>) -> Map<String, Value> {
    Map::from_iter([
        ("rules".to_string(), Value::Object(rules)),
        ("session".to_string(), Value::Object(session)),
    ])
}

pub fn validate_doc(doc: &AgentDoc, fragments: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    let valid_name = doc
        .name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase())
        && doc
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid_name {
        problems.push(format!(
            "name '{}' must start with a lowercase letter and use only lowercase letters, digits and _",
            doc.name
        ));
    }
    if doc.description.trim().is_empty() {
        problems.push("description is empty".to_string());
    }
    if doc.model.trim().is_empty() {
        problems.push("model is empty".to_string());
    }
    if doc.system_prompt.trim().is_empty() {
        problems.push("system_prompt is empty".to_string());
    }
    if doc.max_iterations == Some(0) {
        problems.push("max_iterations must be at least 1".to_string());
    }
    for pattern in &doc.tools {
        problems.extend(tool_pattern_problem(pattern));
    }
    if let Some(schema) = &doc.input_schema {
        problems.extend(input_schema_problems(schema));
    }
    if let Some(schema) = &doc.output_schema {
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            problems.push("output_schema must be an object schema (type: object)".to_string());
        }
    }
    problems.extend(prompt_problems(&doc.system_prompt, fragments));
    problems
}

fn tool_pattern_problem(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return Some("a tools entry is empty".to_string());
    }
    if !pattern
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '*')
    {
        return Some(format!(
            "tools entry '{pattern}' may use only letters, digits, _, - and *"
        ));
    }
    if RESERVED_TOOL_NAMES.contains(&pattern) {
        return Some(format!(
            "tools entry '{pattern}' is a plan step, not a tool an agent can call"
        ));
    }
    if let Some(prefix) = GENERATED_TOOL_PREFIXES
        .iter()
        .find(|prefix| pattern.starts_with(*prefix))
    {
        return Some(format!(
            "tools entry '{pattern}' uses the generated `{prefix}` names; list the agent under `subagents` or `handoffs` instead"
        ));
    }
    None
}

fn input_schema_problems(schema: &Value) -> Vec<String> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return vec!["input_schema must be an object schema (type: object)".to_string()];
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return vec!["input_schema must declare its properties".to_string()];
    };
    properties
        .iter()
        .filter(|(_, property)| {
            property
                .get("description")
                .and_then(Value::as_str)
                .is_none_or(|text| text.trim().is_empty())
        })
        .map(|(key, _)| format!("input_schema property '{key}' needs a description"))
        .collect()
}

fn prompt_problems(prompt: &str, fragments: &[&str]) -> Vec<String> {
    let roots = match referenced_roots(prompt) {
        Ok(roots) => roots,
        Err(error) => return vec![format!("system_prompt is not a valid template: {error}")],
    };
    let unknown: Vec<String> = roots
        .into_iter()
        .filter(|root| !TEMPLATE_ROOTS.contains(&root.as_str()))
        .collect();
    if !unknown.is_empty() {
        return vec![format!(
            "system_prompt references {}; agent prompts may reference only {}. The template language has no escaping, so a literal {{{{…}}}} example can't appear in a prompt — teach template syntax with {{{{rules.templating}}}}",
            unknown
                .iter()
                .map(|root| format!("`{root}`"))
                .collect::<Vec<_>>()
                .join(", "),
            TEMPLATE_ROOTS
                .iter()
                .map(|root| format!("`{root}.*`"))
                .collect::<Vec<_>>()
                .join(" and ")
        )];
    }
    let rules: Map<String, Value> = fragments
        .iter()
        .map(|fragment| (fragment.to_string(), Value::String(String::new())))
        .collect();
    let session: Map<String, Value> = SESSION_KEYS
        .iter()
        .map(|key| (key.to_string(), Value::String(String::new())))
        .collect();
    let scope = template_scope(rules, session);
    match render_str(prompt, &Roots::new(&scope)) {
        Ok(_) => Vec::new(),
        Err(error) => vec![format!(
            "system_prompt: {error} (known fragments: {}; session values: {})",
            fragments
                .iter()
                .map(|f| format!("rules.{f}"))
                .collect::<Vec<_>>()
                .join(", "),
            SESSION_KEYS
                .iter()
                .map(|k| format!("session.{k}"))
                .collect::<Vec<_>>()
                .join(", ")
        )],
    }
}

pub fn input_problems(schema: &Value, input: &Value) -> Vec<String> {
    match jsonschema::validator_for(schema) {
        Ok(validator) => validator
            .iter_errors(input)
            .map(|e| e.to_string())
            .collect(),
        Err(error) => vec![error.to_string()],
    }
}

pub fn render_system_prompt(
    doc: &AgentDoc,
    fragments: &BTreeMap<&str, &str>,
    session: &BTreeMap<&str, String>,
) -> Result<String, String> {
    let rules: Map<String, Value> = fragments
        .iter()
        .map(|(name, text)| (name.to_string(), Value::String(text.to_string())))
        .collect();
    let session: Map<String, Value> = session
        .iter()
        .map(|(key, value)| (key.to_string(), Value::String(value.clone())))
        .collect();
    let scope = template_scope(rules, session);
    render_str(&doc.system_prompt, &Roots::new(&scope))
        .map_err(|error| format!("agent '{}': system_prompt: {error}", doc.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> AgentDoc {
        parse_agent_source(yaml).unwrap()
    }

    fn minimal(name: &str, extra: &str) -> String {
        format!("name: {name}\ndescription: d\nmodel: chat\ntools: []\nsystem_prompt: p\n{extra}")
    }

    #[test]
    fn unknown_keys_are_rejected_by_name() {
        let err = parse_agent_source(&minimal("a", "prompt: hi\n")).unwrap_err();
        assert!(err.contains("`prompt`"), "{err}");
    }

    #[test]
    fn a_newer_version_is_refused() {
        let err = parse_agent_source(&format!("version: 2\n{}", minimal("a", ""))).unwrap_err();
        assert!(err.contains("agent"), "{err}");
    }

    #[test]
    fn required_keys_are_required() {
        let err = parse_agent_source("name: a\ndescription: d\nmodel: chat\nsystem_prompt: p\n")
            .unwrap_err();
        assert!(err.contains("tools"), "{err}");
    }

    #[test]
    fn the_builtin_chat_agent_parses_and_validates() {
        let (set, errors) = AgentSet::load(BUILTINS, &[]);
        assert!(errors.is_empty(), "{errors:?}");
        let fragments: Vec<&str> = global_fragments().into_keys().collect();
        let problems = set.validate(&fragments);
        assert!(problems.is_empty(), "{problems:?}");
        let chat = set.get("chat").unwrap();
        assert_eq!(chat.subagents, ["chat"]);
        assert_eq!(chat.source, AgentSource::Builtin);
    }

    #[test]
    fn schemas_are_optional_and_independent() {
        let only_output = minimal(
            "a",
            "output_schema: {type: object, properties: {x: {type: string}}}\n",
        );
        let set = AgentSet::layered(vec![doc(&only_output)], vec![]);
        assert!(set.validate(&[]).is_empty());
        let a = set.get("a").unwrap();
        assert_eq!(a.subagent_input_schema()["required"], json!(["prompt"]));
        assert_eq!(
            a.subagent_output_schema()["properties"]["x"]["type"],
            "string"
        );
    }

    #[test]
    fn input_schema_properties_need_descriptions() {
        let bad = minimal(
            "a",
            "input_schema: {type: object, properties: {goal: {type: string}}}\n",
        );
        let problems = AgentSet::layered(vec![doc(&bad)], vec![]).validate(&[]);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("'goal' needs a description")),
            "{problems:?}"
        );
    }

    #[test]
    fn delegation_targets_must_exist_and_handoffs_cannot_target_self() {
        let a = minimal("a", "subagents: [ghost, a]\nhandoffs: [a, nobody]\n");
        let problems = AgentSet::layered(vec![doc(&a)], vec![]).validate(&[]);
        let joined = problems.join("\n");
        assert!(joined.contains("subagent 'ghost'"), "{joined}");
        assert!(
            !joined.contains("subagent 'a'"),
            "self-spawn is allowed: {joined}"
        );
        assert!(joined.contains("hand off to itself"), "{joined}");
        assert!(joined.contains("handoff 'nobody'"), "{joined}");
    }

    #[test]
    fn subagent_cycles_other_than_self_spawn_are_rejected() {
        let a = minimal("a", "subagents: [a, b]\n");
        let b = minimal("b", "subagents: [c]\n");
        let c = minimal("c", "subagents: [a]\n");
        let problems = AgentSet::layered(vec![doc(&a), doc(&b), doc(&c)], vec![]).validate(&[]);
        let cycles: Vec<&String> = problems.iter().filter(|p| p.contains("cycle")).collect();
        assert_eq!(cycles.len(), 1, "{problems:?}");
        assert!(cycles[0].contains("a → b → c → a"), "{cycles:?}");
    }

    #[test]
    fn tool_patterns_exclude_step_names_and_generated_names() {
        let a = minimal("a", "").replace(
            "tools: []",
            "tools: [map, agent__b, transfer_to_c, 'bad name', \"linear__*\"]",
        );
        let problems = AgentSet::layered(vec![doc(&a)], vec![]).validate(&[]);
        let joined = problems.join("\n");
        assert!(joined.contains("'map' is a plan step"), "{joined}");
        assert!(joined.contains("'agent__b' uses the generated"), "{joined}");
        assert!(
            joined.contains("'transfer_to_c' uses the generated"),
            "{joined}"
        );
        assert!(joined.contains("'bad name' may use only"), "{joined}");
        assert!(!joined.contains("linear__*"), "{joined}");
    }

    #[test]
    fn prompts_may_reference_only_rules_and_session() {
        let a = minimal("a", "").replace("system_prompt: p", "system_prompt: 'see {{E0.values}}'");
        let problems = AgentSet::layered(vec![doc(&a)], vec![]).validate(&["templating"]);
        assert!(
            problems[0].contains("`E0`") && problems[0].contains("no escaping"),
            "{problems:?}"
        );
        let b = minimal("b", "").replace(
            "system_prompt: p",
            "system_prompt: '{{rules.nope}} {{session.date}}'",
        );
        let problems = AgentSet::layered(vec![doc(&b)], vec![]).validate(&["templating"]);
        assert!(problems[0].contains("rules.templating"), "{problems:?}");
    }

    #[test]
    fn later_layers_replace_earlier_ones_whole() {
        let builtin = doc(&minimal("chat", "subagents: [chat]\n"));
        let global = doc(&minimal("chat", "").replace("description: d", "description: global"));
        let project = doc(&minimal("chat", "").replace("description: d", "description: project"));
        let set = AgentSet::layered(vec![builtin], vec![vec![global], vec![project]]);
        let chat = set.get("chat").unwrap();
        assert_eq!(chat.description, "project");
        assert!(chat.subagents.is_empty(), "no field-level merge");
    }

    #[test]
    fn rendering_fills_fragments_and_session_values() {
        let a = doc(&minimal("a", "").replace(
            "system_prompt: p",
            "system_prompt: \"{{rules.templating}} on {{session.date}}\"",
        ));
        let fragments = BTreeMap::from([("templating", "use {{E0}}")]);
        let session = BTreeMap::from([("date", "today".to_string()), ("user", String::new())]);
        assert_eq!(
            render_system_prompt(&a, &fragments, &session).unwrap(),
            "use {{E0}} on today"
        );
    }

    #[test]
    fn a_directory_layer_reports_duplicates_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), minimal("dup", "")).unwrap();
        std::fs::write(dir.path().join("b.yaml"), minimal("dup", "")).unwrap();
        std::fs::write(dir.path().join("c.yml"), "name: [").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let loaded = load_agent_dir(dir.path());
        assert_eq!(loaded.docs.len(), 1);
        assert_eq!(loaded.errors.len(), 2, "{:?}", loaded.errors);
        assert!(loaded.errors[0].contains("duplicate agent name 'dup'"));
    }
}
