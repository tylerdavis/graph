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

pub const ARTIFACT_TOOLS: [&str; 4] = [SET_ARTIFACT, TRY_ARTIFACT, SAVE_ARTIFACT, DISCARD_ARTIFACT];

pub const TOOL_DRAFTER: &str = "tool_drafter";
pub const AGENT_DRAFTER: &str = "agent_drafter";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Agent,
    Tool,
}

impl ArtifactKind {
    fn parse(text: &str) -> Option<Self> {
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
    }
    let problems = check(pipeline, kind, yaml).await;
    let artifact = Artifact {
        kind,
        yaml: yaml.to_string(),
        problems: problems.clone(),
        last_run: None,
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
    outcome(run, false)
}

pub async fn save_artifact(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<Msg>,
    overwrite: bool,
    untested: bool,
) -> Result<String, String> {
    let Some(artifact) = draft.lock().unwrap().artifact.clone() else {
        return Err("there is no draft to save".to_string());
    };
    let saved = match artifact.kind {
        ArtifactKind::Tool => {
            pipeline
                .save_tool_file(&artifact.yaml, untested, overwrite, false)
                .await
        }
        ArtifactKind::Agent => {
            pipeline
                .save_agent_file(&artifact.yaml, overwrite, false)
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
