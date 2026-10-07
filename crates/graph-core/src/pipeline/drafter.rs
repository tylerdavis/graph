use super::gate::StepPath;
use super::interlocutor::{AskOutcome, AskRequest};
use super::{Pipeline, ToolCatalog};
use crate::agent::doc::{global_fragments, parse_agent_source, validate_doc, AgentDoc, AgentSet};
use crate::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use crate::user_tools::{
    parse_tool_source, validate_tool, ToolKind, UserToolDoc, UserToolRegistry, USER_TOOL_PREFIX,
};
use async_trait::async_trait;
use graph_llm::ModelRouter;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub const TRY_TOOL_TOOL: &str = "builtin__try_tool";

pub const SAVE_TOOL_TOOL: &str = "builtin__save_tool";

pub const SAVE_AGENT_TOOL: &str = "builtin__save_agent";

const WORKBENCH_AGENTS: [&str; 3] = ["orchestrator", "plan_loader", "plan_editor"];

#[derive(Debug, Default)]
pub struct Drafted {
    tools: RwLock<Vec<UserToolDoc>>,
    agents: Arc<RwLock<Vec<AgentDoc>>>,
    tested: Mutex<HashSet<String>>,
    pub tools_dir: Option<PathBuf>,
    pub agents_dir: Option<PathBuf>,
}

impl Drafted {
    pub fn new(tools_dir: Option<PathBuf>, agents_dir: Option<PathBuf>) -> Self {
        Self {
            tools_dir,
            agents_dir,
            ..Self::default()
        }
    }

    pub fn tools(&self) -> Vec<UserToolDoc> {
        self.tools.read().unwrap().clone()
    }

    pub fn agents(&self) -> Arc<RwLock<Vec<AgentDoc>>> {
        self.agents.clone()
    }

    fn add_tool(&self, doc: UserToolDoc) {
        let mut tools = self.tools.write().unwrap();
        tools.retain(|existing| existing.name != doc.name);
        tools.push(doc);
    }

    fn add_agent(&self, doc: AgentDoc) {
        let mut agents = self.agents.write().unwrap();
        agents.retain(|existing| existing.name != doc.name);
        agents.push(doc);
    }
}

pub struct DraftedTools {
    drafted: Arc<Drafted>,
    router: Arc<ModelRouter>,
}

impl DraftedTools {
    pub fn new(drafted: Arc<Drafted>, router: Arc<ModelRouter>) -> Self {
        Self { drafted, router }
    }

    fn registry(&self) -> UserToolRegistry {
        UserToolRegistry::new(self.drafted.tools(), self.router.clone())
    }
}

#[async_trait]
impl ToolRegistry for DraftedTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        self.registry().tools().await
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        self.registry().invoke(name, input).await
    }
}

pub fn drafter_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TRY_TOOL_TOOL.to_string(),
            description: "Test-runs a drafted user tool once, before it is saved. Checks the \
                          tool file, asks the user to approve the run (showing what will run), \
                          then runs it on the sample input and records its output shape. Nothing \
                          runs without the user's yes."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["yaml", "input"],
                "properties": {
                    "yaml": {"type": "string", "description": "The whole user tool file"},
                    "input": {"type": "object", "description": "Sample input for one run, matching the tool's input_schema"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["ran"],
                "properties": {
                    "ran": {"type": "boolean"},
                    "reason": {"type": "string"},
                    "problems": {"type": "array", "items": {"type": "string"}},
                    "is_error": {"type": "boolean"},
                    "result": {},
                    "shape": {"type": "object"}
                }
            })),
            output_example: None,
            read_only: Some(false),
        },
        ToolDef {
            name: SAVE_TOOL_TOOL.to_string(),
            description: "Saves a drafted user tool to the project's tools directory and makes \
                          it available as user__<name> right away. Refuses a file that hasn't \
                          passed builtin__try_tool unless `untested` is true, which is only for \
                          when the user said not to test it. Refuses to replace an existing file \
                          unless `overwrite` is true."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["yaml"],
                "properties": {
                    "yaml": {"type": "string", "description": "The whole user tool file, exactly as tested"},
                    "untested": {"type": "boolean", "description": "Save without a test run, only when the user said not to test it"},
                    "overwrite": {"type": "boolean", "description": "Replace an existing file with the same name, only after the user confirms"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "saved": {"type": "string"},
                    "path": {"type": "string"},
                    "untested": {"type": "boolean"},
                    "error": {"type": "string"},
                    "problems": {"type": "array", "items": {"type": "string"}}
                }
            })),
            output_example: None,
            read_only: Some(false),
        },
        ToolDef {
            name: SAVE_AGENT_TOOL.to_string(),
            description: "Validates a drafted agent file (keys, model role, tool patterns, \
                          subagents and handoffs, schemas, prompt templates), saves it to the \
                          project's agents directory, and makes it available right away. \
                          Refuses to replace an existing file unless `overwrite` is true."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["yaml"],
                "properties": {
                    "yaml": {"type": "string", "description": "The whole agent file"},
                    "overwrite": {"type": "boolean", "description": "Replace an existing file with the same name, only after the user confirms"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "saved": {"type": "string"},
                    "path": {"type": "string"},
                    "error": {"type": "string"},
                    "problems": {"type": "array", "items": {"type": "string"}}
                }
            })),
            output_example: None,
            read_only: Some(false),
        },
    ]
}

fn refused(error: impl Into<String>, problems: Vec<String>) -> Value {
    json!({"error": error.into(), "problems": problems})
}

fn parse_tool(yaml: &str) -> Result<UserToolDoc, Vec<String>> {
    let doc = parse_tool_source(yaml).map_err(|error| vec![error])?;
    validate_tool(&doc).map_err(|error| vec![error])?;
    Ok(doc)
}

fn run_preview(doc: &UserToolDoc, input: &Value) -> String {
    let what = match &doc.kind {
        ToolKind::Exec { command, args, .. } => {
            let mut line = command.clone();
            for arg in args {
                line.push(' ');
                line.push_str(arg);
            }
            format!("runs: {line}")
        }
        ToolKind::Prompt { .. } => "makes one model call with its prompt".to_string(),
        ToolKind::Reshape { .. } => "reshapes its input, with no side effects".to_string(),
        ToolKind::Decision { .. } => "asks the decision model its questions".to_string(),
    };
    format!(
        "Test-run the drafted tool {USER_TOOL_PREFIX}{} once? It {what}\n\nwith input: {}",
        doc.name,
        serde_json::to_string_pretty(input).unwrap_or_default()
    )
}

fn write_file(dir: &Path, name: &str, yaml: &str, overwrite: bool) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{name}.yaml"));
    if path.exists() && !overwrite {
        return Err(format!(
            "{} already exists; pass overwrite: true only after the user confirms replacing it",
            path.display()
        ));
    }
    let staging = dir.join(format!(".{name}.yaml.tmp"));
    std::fs::write(&staging, yaml)
        .map_err(|e| format!("can't write {}: {e}", staging.display()))?;
    std::fs::rename(&staging, &path)
        .map_err(|e| format!("can't move {} into place: {e}", path.display()))?;
    Ok(path)
}

fn stamped(yaml: &str, kind: crate::format::Kind) -> String {
    match serde_yaml::from_str::<serde_yaml::Value>(yaml) {
        Ok(mut value) if value.get("version").is_none() => {
            crate::format::stamp(kind, &mut value);
            serde_yaml::to_string(&value).unwrap_or_else(|_| yaml.to_string())
        }
        _ => yaml.to_string(),
    }
}

impl Pipeline {
    pub async fn call_drafter_tool(&self, name: &str, input: Value) -> Option<ToolOutcome> {
        if ![TRY_TOOL_TOOL, SAVE_TOOL_TOOL, SAVE_AGENT_TOOL].contains(&name) {
            return None;
        }
        Some(self.call_native(name, input).await)
    }

    pub fn live_catalog(&self) -> Option<ToolCatalog> {
        let mut catalog = self.catalog.as_deref()?.clone();
        for doc in self.drafted.tools() {
            catalog
                .user_tools
                .insert(format!("{USER_TOOL_PREFIX}{}", doc.name));
        }
        for doc in self.drafted.agents().read().unwrap().iter() {
            catalog.agents.insert(doc.name.clone());
        }
        Some(catalog)
    }

    pub(super) async fn try_tool(&self, input: Value) -> Result<Value, String> {
        let yaml = input["yaml"].as_str().unwrap_or_default();
        let doc = match parse_tool(yaml) {
            Ok(doc) => doc,
            Err(problems) => return Ok(json!({"ran": false, "problems": problems})),
        };
        let sample = match &input["input"] {
            Value::Object(_) => input["input"].clone(),
            _ => json!({}),
        };
        let Some(interlocutor) = &self.interlocutor else {
            return Ok(json!({
                "ran": false,
                "reason": "nobody can approve a test run here; ask the user whether to save it untested"
            }));
        };
        let outcome = interlocutor
            .ask(AskRequest {
                path: StepPath::top("try_tool"),
                call_stack: self.call_stack.clone(),
                prompt: run_preview(&doc, &sample),
                schema: json!({
                    "type": "object",
                    "required": ["run"],
                    "properties": {"run": {"type": "boolean", "description": "Run it now?"}}
                }),
                default: Some(json!({"run": true})),
            })
            .await;
        let approved = match outcome {
            AskOutcome::Answered(answer) => answer["run"] == json!(true),
            AskOutcome::Declined => false,
            AskOutcome::Unavailable(reason) => {
                return Ok(
                    json!({"ran": false, "reason": format!("the user couldn't be asked: {reason}")}),
                )
            }
        };
        if !approved {
            return Ok(json!({"ran": false, "reason": "the user declined the test run"}));
        }
        let name = format!("{USER_TOOL_PREFIX}{}", doc.name);
        let registry = UserToolRegistry::new(vec![doc], self.router.clone());
        let outcome = registry
            .invoke(&name, sample)
            .await
            .map_err(|error| error.to_string())?;
        let mut report =
            json!({"ran": true, "is_error": outcome.is_error, "result": outcome.result});
        if !outcome.is_error {
            let shape = crate::shapes::infer_schema(&outcome.result);
            if let Some(store) = &self.store {
                let example = crate::shapes::truncate_example(&outcome.result);
                if let Err(error) = store.record_tool_shape(&name, &shape, &example).await {
                    tracing::debug!(tool = %name, error = %error, "shape recording failed");
                }
            }
            report["shape"] = shape;
            self.drafted.tested.lock().unwrap().insert(yaml.to_string());
        }
        Ok(report)
    }

    pub(super) async fn save_tool(&self, input: Value) -> Result<Value, String> {
        Ok(self
            .save_tool_file(
                input["yaml"].as_str().unwrap_or_default(),
                input["untested"].as_bool().unwrap_or(false),
                input["overwrite"].as_bool().unwrap_or(false),
                true,
            )
            .await)
    }

    pub(super) async fn save_agent(&self, input: Value) -> Result<Value, String> {
        Ok(self
            .save_agent_file(
                input["yaml"].as_str().unwrap_or_default(),
                input["overwrite"].as_bool().unwrap_or(false),
                true,
            )
            .await)
    }

    pub async fn try_tool_file(&self, yaml: &str, input: Value) -> Value {
        self.try_tool(json!({"yaml": yaml, "input": input}))
            .await
            .unwrap_or_else(|error| json!({"ran": false, "problems": [error]}))
    }

    pub fn tool_was_tested(&self, yaml: &str) -> bool {
        self.drafted.tested.lock().unwrap().contains(yaml)
    }

    pub fn check_tool_file(&self, yaml: &str) -> Vec<String> {
        parse_tool(yaml).err().unwrap_or_default()
    }

    async fn confirm_save(&self, name: &str, path: &Path) -> Result<(), Value> {
        let Some(interlocutor) = &self.interlocutor else {
            return Ok(());
        };
        let outcome = interlocutor
            .ask(AskRequest {
                path: StepPath::top("save"),
                call_stack: self.call_stack.clone(),
                prompt: format!("Save {name} to {}?", path.display()),
                schema: json!({
                    "type": "object",
                    "required": ["save"],
                    "properties": {"save": {"type": "boolean", "description": "Save it now?"}}
                }),
                default: Some(json!({"save": true})),
            })
            .await;
        match outcome {
            AskOutcome::Answered(answer) if answer["save"] == json!(true) => Ok(()),
            AskOutcome::Unavailable(_) => Ok(()),
            _ => Err(refused("the user didn't want it saved yet", Vec::new())),
        }
    }

    pub async fn save_tool_file(
        &self,
        yaml: &str,
        untested: bool,
        overwrite: bool,
        confirm: bool,
    ) -> Value {
        let doc = match parse_tool(yaml) {
            Ok(doc) => doc,
            Err(problems) => return refused("the tool file is not valid", problems),
        };
        if !untested && !self.tool_was_tested(yaml) {
            return refused(
                "this exact file hasn't passed a test run: test it first, or save it untested if the user said not to test it",
                Vec::new(),
            );
        }
        let Some(dir) = self.drafted.tools_dir.clone() else {
            return refused(
                "no tools directory is configured ([tools].paths)",
                Vec::new(),
            );
        };
        let name = format!("{USER_TOOL_PREFIX}{}", doc.name);
        let target = dir.join(format!("{}.yaml", doc.name));
        let defined_elsewhere = self
            .catalog
            .as_deref()
            .is_some_and(|catalog| catalog.user_tools.contains(&name))
            && !target.exists();
        if defined_elsewhere {
            return refused(
                format!("a tool named {name} is already defined in another tools directory; pick another name"),
                Vec::new(),
            );
        }
        if target.exists() && !overwrite {
            return refused(
                format!("{} already exists; pass overwrite: true only after the user confirms replacing it", target.display()),
                Vec::new(),
            );
        }
        if confirm {
            if let Err(declined) = self.confirm_save(&name, &target).await {
                return declined;
            }
        }
        let path = match write_file(
            &dir,
            &doc.name,
            &stamped(yaml, crate::format::Kind::Tool),
            overwrite,
        ) {
            Ok(path) => path,
            Err(error) => return refused(error, Vec::new()),
        };
        self.drafted.add_tool(doc);
        json!({"saved": name, "path": path.display().to_string(), "untested": untested})
    }

    pub async fn check_agent_file(&self, yaml: &str) -> Result<AgentDoc, Vec<String>> {
        let doc = parse_agent_source(yaml).map_err(|error| vec![error])?;
        if WORKBENCH_AGENTS.contains(&doc.name.as_str()) {
            return Err(vec![format!(
                "'{}' is a workbench agent; drafting workbench agents isn't supported",
                doc.name
            )]);
        }
        let fragments = global_fragments();
        let names: Vec<&str> = fragments.keys().copied().collect();
        let mut problems = validate_doc(&doc, &names);
        if let Err(error) = self.router.resolve_named(&doc.model) {
            problems.push(format!("model: {error}"));
        }
        let defs = self.planner_tool_defs().await;
        for pattern in &doc.tools {
            let matches = defs
                .iter()
                .any(|def| super::catalog::glob_matches(pattern, &def.name));
            if !matches {
                let stem = pattern.trim_end_matches('*');
                let close: Vec<&str> = defs
                    .iter()
                    .map(|def| def.name.as_str())
                    .filter(|name| name.contains(stem.split("__").last().unwrap_or(stem)))
                    .take(5)
                    .collect();
                problems.push(format!(
                    "tools: '{pattern}' matches no tool in the catalog{}",
                    if close.is_empty() {
                        String::new()
                    } else {
                        format!(" (close: {})", close.join(", "))
                    }
                ));
            }
        }
        let existing: Vec<AgentDoc> = self.agents.iter().collect();
        let layered = AgentSet::layered(existing, vec![vec![doc.clone()]]);
        let referenced: Vec<String> = layered
            .agent_problems(&doc.name, &names)
            .into_iter()
            .filter(|problem| {
                !problems
                    .iter()
                    .any(|known| problem.ends_with(known.as_str()))
            })
            .collect();
        problems.extend(referenced);
        if problems.is_empty() {
            Ok(doc)
        } else {
            Err(problems)
        }
    }

    pub async fn save_agent_file(&self, yaml: &str, overwrite: bool, confirm: bool) -> Value {
        let doc = match self.check_agent_file(yaml).await {
            Ok(doc) => doc,
            Err(problems) => return refused("the agent file has problems", problems),
        };
        let Some(dir) = self.drafted.agents_dir.clone() else {
            return refused("no agents directory is available", Vec::new());
        };
        let target = dir.join(format!("{}.yaml", doc.name));
        if target.exists() && !overwrite {
            return refused(
                format!("{} already exists; pass overwrite: true only after the user confirms replacing it", target.display()),
                Vec::new(),
            );
        }
        if confirm {
            if let Err(declined) = self.confirm_save(&doc.name, &target).await {
                return declined;
            }
        }
        let path = match write_file(
            &dir,
            &doc.name,
            &stamped(yaml, crate::format::Kind::Agent),
            overwrite,
        ) {
            Ok(path) => path,
            Err(error) => return refused(error, Vec::new()),
        };
        let name = doc.name.clone();
        self.drafted.add_agent(doc);
        json!({"saved": name, "path": path.display().to_string()})
    }
}
