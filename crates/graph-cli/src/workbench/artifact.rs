use super::app::Msg;
use super::runner::UiInterlocutor;
use super::tools::SharedDraft;
use graph_core::agent::conversation::HandoffGuard;
use graph_core::pipeline::Pipeline;
use graph_core::{ToolDef, ToolOutcome};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub const SET_ARTIFACT: &str = "workbench__set_artifact";
pub const TRY_ARTIFACT: &str = "workbench__try_artifact";
pub const SAVE_ARTIFACT: &str = "workbench__save_artifact";
pub const DISCARD_ARTIFACT: &str = "workbench__discard_artifact";
pub const LOAD_ARTIFACT: &str = "workbench__load_artifact";

pub const LIST_AGENTS: &str = "workbench__list_agents";
pub const LIST_TOOLS: &str = "workbench__list_tools";

pub const ARTIFACT_TOOLS: [&str; 7] = [
    SET_ARTIFACT,
    TRY_ARTIFACT,
    SAVE_ARTIFACT,
    DISCARD_ARTIFACT,
    LOAD_ARTIFACT,
    LIST_AGENTS,
    LIST_TOOLS,
];

pub const TOOL_DRAFTER: &str = "tool_drafter";
pub const AGENT_DRAFTER: &str = "agent_drafter";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Agent,
    Tool,
}

impl ArtifactKind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "agent" => Some(Self::Agent),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Tool => "tool",
        }
    }

    fn drafter(self) -> &'static str {
        match self {
            Self::Agent => AGENT_DRAFTER,
            Self::Tool => TOOL_DRAFTER,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub yaml: String,
    pub problems: Vec<String>,
    pub last_run: Option<Value>,
    pub editing: Option<std::path::PathBuf>,
    pub original: Option<String>,
}

impl Artifact {
    pub fn name(&self) -> String {
        serde_yaml::from_str::<serde_yaml::Value>(&self.yaml)
            .ok()
            .and_then(|doc| {
                doc.get("name")
                    .and_then(|name| name.as_str().map(str::to_string))
            })
            .unwrap_or_else(|| "unnamed".to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactView {
    pub kind: ArtifactKind,
    pub yaml: String,
    pub problems: Vec<String>,
    pub last_run: Option<Value>,
    pub tested: bool,
    pub parked: Option<String>,
}

fn is_drafter(name: &str) -> bool {
    name == TOOL_DRAFTER || name == AGENT_DRAFTER
}

pub fn view(draft: &SharedDraft, pipeline: &Pipeline) -> Option<ArtifactView> {
    let state = draft.lock().unwrap();
    let artifact = state.artifact.as_ref()?;
    Some(ArtifactView {
        kind: artifact.kind,
        yaml: artifact.yaml.clone(),
        problems: artifact.problems.clone(),
        last_run: artifact.last_run.clone(),
        tested: artifact.kind == ArtifactKind::Tool && pipeline.tool_was_tested(&artifact.yaml),
        parked: state
            .parked
            .as_ref()
            .map(|parked| format!("{} draft '{}'", parked.kind.label(), parked.name())),
    })
}

fn publish(draft: &SharedDraft, pipeline: &Pipeline, tx: &UnboundedSender<Msg>) {
    let _ = tx.send(Msg::ArtifactChanged(view(draft, pipeline).map(Box::new)));
}

fn outcome(result: Value, is_error: bool) -> ToolOutcome {
    ToolOutcome { result, is_error }
}

async fn check(pipeline: &Pipeline, kind: ArtifactKind, yaml: &str) -> Vec<String> {
    match kind {
        ArtifactKind::Tool => pipeline.check_tool_file(yaml),
        ArtifactKind::Agent => pipeline
            .check_agent_file(yaml)
            .await
            .err()
            .unwrap_or_default(),
    }
}

pub async fn set_artifact(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<Msg>,
    input: &Value,
) -> ToolOutcome {
    let Some(kind) = input["kind"].as_str().and_then(ArtifactKind::parse) else {
        return outcome(json!({"error": "kind must be \"agent\" or \"tool\""}), true);
    };
    let Some(yaml) = input["yaml"]
        .as_str()
        .filter(|yaml| !yaml.trim().is_empty())
    else {
        return outcome(json!({"error": "yaml must be the whole file"}), true);
    };
    if let Some(open) = &draft.lock().unwrap().artifact {
        if open.kind != kind {
            return outcome(
                json!({"error": format!("a {} draft is open; save or discard it first", open.kind.label())}),
                true,
            );
        }
        let incoming = Artifact {
            kind,
            yaml: yaml.to_string(),
            problems: Vec::new(),
            last_run: None,
            editing: None,
            original: None,
        }
        .name();
        let current = open.name();
        if incoming != current && current != "unnamed" {
            return outcome(
                json!({"error": format!(
                    "the {} draft '{current}' is still open; save or discard it before starting '{incoming}'",
                    kind.label()
                )}),
                true,
            );
        }
    }
    let problems = check(pipeline, kind, yaml).await;
    let (editing, original) = draft
        .lock()
        .unwrap()
        .artifact
        .as_ref()
        .map(|open| (open.editing.clone(), open.original.clone()))
        .unwrap_or_default();
    let artifact = Artifact {
        kind,
        yaml: yaml.to_string(),
        problems: problems.clone(),
        last_run: None,
        editing,
        original,
    };
    let name = artifact.name();
    draft.lock().unwrap().artifact = Some(artifact);
    publish(draft, pipeline, tx);
    outcome(json!({"name": name, "problems": problems}), false)
}

pub async fn try_artifact(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<Msg>,
    input: &Value,
) -> ToolOutcome {
    let yaml = match &draft.lock().unwrap().artifact {
        Some(artifact) if artifact.kind == ArtifactKind::Tool => artifact.yaml.clone(),
        Some(_) => return outcome(json!({"error": "only a tool draft can be test-run"}), true),
        None => {
            return outcome(
                json!({"error": "there is no tool draft to test; set one first"}),
                true,
            )
        }
    };
    let asking = pipeline
        .clone()
        .with_interlocutor(Arc::new(UiInterlocutor::new(tx.clone())));
    let sample = match &input["input"] {
        Value::Object(_) => input["input"].clone(),
        _ => json!({}),
    };
    let run = asking.try_tool_file(&yaml, sample).await;
    if let Some(artifact) = draft.lock().unwrap().artifact.as_mut() {
        if artifact.yaml == yaml {
            artifact.last_run = Some(run.clone());
        }
    }
    publish(draft, pipeline, tx);
    outcome(for_the_model(run), false)
}

const SAMPLE_CHARS: usize = 1500;

fn for_the_model(run: Value) -> Value {
    if run["ran"] != json!(true) {
        return run;
    }
    let full = serde_json::to_string_pretty(&run["result"]).unwrap_or_default();
    let total = full.chars().count();
    let mut trimmed = json!({
        "ran": true,
        "is_error": run["is_error"],
        "shape": run["shape"],
    });
    if total <= SAMPLE_CHARS {
        trimmed["result"] = run["result"].clone();
    } else {
        trimmed["sample"] = json!(full.chars().take(SAMPLE_CHARS).collect::<String>());
        trimmed["note"] = json!(format!(
            "the result is {total} characters; this is the first {SAMPLE_CHARS}. Its shape above covers the whole result, and the user sees all of it in the pane."
        ));
    }
    trimmed
}

pub async fn save_artifact(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<Msg>,
    overwrite: bool,
    untested: bool,
    confirm: bool,
) -> Result<String, String> {
    let Some(artifact) = draft.lock().unwrap().artifact.clone() else {
        return Err("there is no draft to save".to_string());
    };
    let replacing = artifact.editing.is_some()
        && artifact.original.as_deref() == Some(artifact.name().as_str());
    let overwrite = overwrite || replacing;
    let existing = artifact
        .editing
        .as_deref()
        .filter(|path| replacing && !path.as_os_str().is_empty());
    let asking;
    let saving = if confirm {
        asking = pipeline
            .clone()
            .with_interlocutor(Arc::new(UiInterlocutor::new(tx.clone())));
        &asking
    } else {
        pipeline
    };
    let saved = match artifact.kind {
        ArtifactKind::Tool => {
            saving
                .save_tool_file_at(&artifact.yaml, untested, overwrite, confirm, existing)
                .await
        }
        ArtifactKind::Agent => {
            saving
                .save_agent_file_at(&artifact.yaml, overwrite, confirm, existing)
                .await
        }
    };
    let Some(name) = saved["saved"].as_str() else {
        let mut message = saved["error"].as_str().unwrap_or("not saved").to_string();
        if let Some(problems) = saved["problems"]
            .as_array()
            .filter(|problems| !problems.is_empty())
        {
            let listed: Vec<String> = problems
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            message.push_str(&format!(": {}", listed.join("; ")));
        }
        return Err(message);
    };
    let name = name.to_string();
    draft.lock().unwrap().artifact = None;
    publish(draft, pipeline, tx);
    Ok(format!(
        "saved {name} to {}",
        saved["path"].as_str().unwrap_or_default()
    ))
}

pub fn existing(
    kind: ArtifactKind,
    name: &str,
    tool_dirs: &[std::path::PathBuf],
) -> Result<Artifact, String> {
    let (yaml, editing) = match kind {
        ArtifactKind::Agent => {
            let (set, _) = graph_core::agent::doc::AgentSet::load(
                graph_core::agent::doc::BUILTINS,
                &graph_core::agent::doc::agent_dirs(),
            );
            let doc = set
                .get(name)
                .ok_or_else(|| format!("there's no agent named '{name}'"))?;
            match &doc.source {
                graph_core::agent::doc::AgentSource::File(path) => (
                    std::fs::read_to_string(path)
                        .map_err(|e| format!("can't read {}: {e}", path.display()))?,
                    Some(path.clone()),
                ),
                graph_core::agent::doc::AgentSource::Builtin => {
                    let mut value = serde_yaml::to_value(&doc).map_err(|e| e.to_string())?;
                    graph_core::format::stamp(graph_core::format::Kind::Agent, &mut value);
                    (
                        serde_yaml::to_string(&value).map_err(|e| e.to_string())?,
                        Some(std::path::PathBuf::new()),
                    )
                }
            }
        }
        ArtifactKind::Tool => {
            let bare = name
                .strip_prefix(graph_core::user_tools::USER_TOOL_PREFIX)
                .unwrap_or(name);
            let docs = graph_core::user_tools::load_user_tools(tool_dirs)?;
            let doc = docs.into_iter().find(|doc| doc.name == bare).ok_or_else(|| {
                format!("there's no user tool named '{bare}'; built-in pack tools can't be edited here")
            })?;
            let path = doc
                .path
                .ok_or_else(|| format!("'{bare}' has no file to edit"))?;
            (
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("can't read {}: {e}", path.display()))?,
                Some(path),
            )
        }
    };
    Ok(Artifact {
        kind,
        yaml,
        problems: Vec::new(),
        last_run: None,
        editing,
        original: Some(match kind {
            ArtifactKind::Agent => name.to_string(),
            ArtifactKind::Tool => name
                .strip_prefix(graph_core::user_tools::USER_TOOL_PREFIX)
                .unwrap_or(name)
                .to_string(),
        }),
    })
}

pub async fn open_existing(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<Msg>,
    kind: ArtifactKind,
    name: &str,
) -> Result<String, String> {
    if draft.lock().unwrap().artifact.is_some() {
        return Err("a draft is already open; save or discard it first".to_string());
    }
    let mut artifact = existing(kind, name, &pipeline.drafted.tool_dirs)?;
    artifact.problems = check(pipeline, kind, &artifact.yaml).await;
    let name = artifact.name();
    draft.lock().unwrap().artifact = Some(artifact);
    publish(draft, pipeline, tx);
    Ok(name)
}

pub fn list_agents(pipeline: &Pipeline) -> ToolOutcome {
    let agents: Vec<Value> = pipeline
        .agents
        .iter()
        .map(|doc| {
            json!({
                "name": doc.name,
                "description": doc.description,
                "source": doc.source.describe(),
                "typed": doc.input_schema.is_some() || doc.output_schema.is_some(),
            })
        })
        .collect();
    outcome(json!({"count": agents.len(), "agents": agents}), false)
}

pub async fn list_tools(pipeline: &Pipeline) -> ToolOutcome {
    let user: Vec<Value> = graph_core::user_tools::load_user_tools(&pipeline.drafted.tool_dirs)
        .unwrap_or_default()
        .into_iter()
        .map(|doc| {
            json!({
                "name": format!("{}{}", graph_core::user_tools::USER_TOOL_PREFIX, doc.name),
                "description": doc.description,
                "path": doc.path.map(|path| path.display().to_string()),
            })
        })
        .collect();
    let servers: Vec<Value> = pipeline
        .registry
        .servers()
        .await
        .into_iter()
        .map(|server| json!({"name": server.name, "description": server.description}))
        .collect();
    outcome(json!({"user_tools": user, "mcp_servers": servers}), false)
}

pub fn discard_artifact(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<Msg>,
) -> Option<String> {
    let discarded = draft.lock().unwrap().artifact.take()?;
    publish(draft, pipeline, tx);
    Some(format!(
        "discarded the {} draft '{}'",
        discarded.kind.label(),
        discarded.name()
    ))
}

pub fn handoff_guard(
    draft: SharedDraft,
    pipeline: Arc<Pipeline>,
    tx: UnboundedSender<Msg>,
) -> HandoffGuard {
    Arc::new(move |from: &str, to: &str| {
        let mut state = draft.lock().unwrap();
        let between_drafters = is_drafter(from) && is_drafter(to);
        if let Some(open) = &state.artifact {
            if between_drafters && state.parked.is_none() {
                state.parked = state.artifact.take();
                drop(state);
                publish(&draft, &pipeline, &tx);
                return None;
            }
            return Some(format!(
                "the {} draft '{}' isn't saved: save it or discard it before handing off",
                open.kind.label(),
                open.name()
            ));
        }
        if state
            .parked
            .as_ref()
            .is_some_and(|parked| parked.kind.drafter() == to)
        {
            state.artifact = state.parked.take();
            drop(state);
            publish(&draft, &pipeline, &tx);
        }
        None
    })
}

pub fn context_section(draft: &SharedDraft) -> String {
    let state = draft.lock().unwrap();
    let mut section = String::from("## Current draft\n");
    match &state.artifact {
        Some(artifact) => {
            section.push_str(&format!(
                "The pane shows this {} draft, current as of this turn:\n{}\n",
                artifact.kind.label(),
                artifact.yaml
            ));
            if artifact.problems.is_empty() {
                section.push_str("It has no problems.\n");
            } else {
                section.push_str(&format!(
                    "Its problems:\n- {}\n",
                    artifact.problems.join("\n- ")
                ));
            }
            if let Some(run) = &artifact.last_run {
                section.push_str(&format!("Its last test run: {run}\n"));
            }
        }
        None => section.push_str("(none yet: put a first draft in the pane)\n"),
    }
    if let Some(parked) = &state.parked {
        section.push_str(&format!(
            "Waiting while you work: the {} draft '{}', which is restored when you hand back.\n",
            parked.kind.label(),
            parked.name()
        ));
    }
    section
}

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: SET_ARTIFACT.to_string(),
            description: "Put a whole agent or tool file in the workbench pane as the current \
                          draft, replacing what's there. It is checked right away and the \
                          problems come back, but nothing is written to disk."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind", "yaml"],
                "properties": {
                    "kind": {"type": "string", "enum": ["agent", "tool"], "description": "What the file is"},
                    "yaml": {"type": "string", "description": "The whole file"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: TRY_ARTIFACT.to_string(),
            description: "Test-run the tool draft in the pane once on a sample input. The user \
                          approves the run first; the result shows in the pane."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["input"],
                "properties": {
                    "input": {"type": "object", "description": "Sample input matching the tool's input_schema"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(false),
        },
        ToolDef {
            name: SAVE_ARTIFACT.to_string(),
            description: "Save the draft in the pane to the project, only when the user asks to \
                          save it. A tool must have passed a test run unless `untested` is true \
                          because the user said not to test it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "overwrite": {"type": "boolean", "description": "Replace an existing file, only after the user confirms"},
                    "untested": {"type": "boolean", "description": "Save a tool without a test run, only when the user said not to test it"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(false),
        },
        ToolDef {
            name: LIST_AGENTS.to_string(),
            description: "List the agents you can chat with or call from plans: name, \
                          description, where each comes from, and whether it takes typed input."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: LIST_TOOLS.to_string(),
            description: "List the user tools (name, description, file) and the connected MCP \
                          servers. Use it when the user asks what tools they have."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: LOAD_ARTIFACT.to_string(),
            description: "Open an existing agent or user tool in the pane for editing. Saving \
                          it later replaces that file (an edited built-in agent is saved as a \
                          project override)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind", "name"],
                "properties": {
                    "kind": {"type": "string", "enum": ["agent", "tool"], "description": "What to open"},
                    "name": {"type": "string", "description": "The agent's name, or the tool's name (with or without user__)"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: DISCARD_ARTIFACT.to_string(),
            description: "Throw away the draft in the pane, only when the user asks to discard it."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_test_result_reaches_the_model_as_its_shape_and_a_sample() {
        let long = "x".repeat(5000);
        let trimmed = for_the_model(json!({
            "ran": true,
            "is_error": false,
            "result": {"text": long},
            "shape": {"type": "object", "properties": {"text": {"type": "string"}}}
        }));
        assert!(trimmed.get("result").is_none());
        assert_eq!(trimmed["shape"]["properties"]["text"]["type"], "string");
        assert_eq!(
            trimmed["sample"].as_str().unwrap().chars().count(),
            SAMPLE_CHARS
        );
        assert!(trimmed["note"].as_str().unwrap().contains("characters"));

        let short = for_the_model(
            json!({"ran": true, "is_error": false, "result": {"ok": 1}, "shape": {}}),
        );
        assert_eq!(short["result"], json!({"ok": 1}));
    }
}
