use super::tools::DraftState;
use graph_core::agent::conversation::ContextHook;
use graph_core::pipeline::authoring;
use graph_core::pipeline::doc::PlanDoc;
use std::sync::{Arc, Mutex};

pub const WORKBENCH_AGENTS: &[&str] = &[
    include_str!("front_desk.yaml"),
    include_str!("plan_drafter.yaml"),
    include_str!("plan_loader.yaml"),
    include_str!("plan_editor.yaml"),
    include_str!("tool_drafter.yaml"),
    include_str!("agent_drafter.yaml"),
];

pub const FRONT_DESK: &str = "front_desk";

pub const PLAN_DRAFTER: &str = "plan_drafter";

pub const PLAN_EDITOR: &str = "plan_editor";

pub fn workbench_only() -> Vec<graph_core::agent::doc::AgentDoc> {
    let globals: Vec<String> = graph_core::agent::doc::BUILTINS
        .iter()
        .filter_map(|raw| graph_core::agent::doc::parse_agent_source(raw).ok())
        .map(|doc| doc.name)
        .collect();
    let (set, _) = graph_core::agent::doc::AgentSet::load(WORKBENCH_AGENTS, &[]);
    set.iter()
        .filter(|doc| !globals.contains(&doc.name))
        .collect()
}

pub fn is_workbench_only(name: &str) -> bool {
    workbench_only().iter().any(|doc| doc.name == name)
}

pub fn builtin_sources() -> Vec<&'static str> {
    graph_core::agent::doc::BUILTINS
        .iter()
        .chain(WORKBENCH_AGENTS)
        .copied()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    FrontDesk,
    Plan(Option<String>),
    Agent(Option<String>),
    Tool(Option<String>),
}

pub fn starting_agent(start: &Start, doc: Option<&PlanDoc>) -> &'static str {
    match (start, doc) {
        (Start::FrontDesk, _) => FRONT_DESK,
        (Start::Plan(_), Some(_)) => PLAN_EDITOR,
        (Start::Plan(_), None) => PLAN_DRAFTER,
        (Start::Agent(_), _) => super::artifact::AGENT_DRAFTER,
        (Start::Tool(_), _) => super::artifact::TOOL_DRAFTER,
    }
}

enum Section {
    CurrentDraft,
    DraftSummary,
    Artifact,
}

fn sections(agent: &str) -> &'static [Section] {
    match agent {
        "plan_editor" => &[Section::CurrentDraft],
        "plan_drafter" => &[Section::DraftSummary],
        "tool_drafter" | "agent_drafter" => &[Section::Artifact],
        _ => &[],
    }
}

pub fn context_hook(draft: Arc<Mutex<DraftState>>) -> ContextHook {
    Arc::new(move |agent: &str| {
        let doc = draft.lock().unwrap().doc.clone();
        sections(agent)
            .iter()
            .map(|section| match section {
                Section::CurrentDraft => current_draft(&doc),
                Section::DraftSummary => draft_summary(&doc),
                Section::Artifact => super::artifact::context_section(&draft),
            })
            .collect()
    })
}

fn current_draft(doc: &Option<PlanDoc>) -> String {
    let mut section = String::from("## Current draft\n");
    match doc {
        Some(doc) => {
            section.push_str(&format!(
                "The plan pane currently shows '{}' — this YAML is current as \
                 of this turn, so do NOT call workbench__get_plan just to read \
                 it (only to re-check after your own edits within this turn):\n",
                doc.identifier
            ));
            match authoring::to_yaml(doc) {
                Ok(yaml) => section.push_str(&yaml),
                Err(_) => section.push_str("(unserializable draft — use workbench__get_plan)"),
            }
        }
        None => section.push_str("(none yet — the pane is empty)"),
    }
    section
}

fn draft_summary(doc: &Option<PlanDoc>) -> String {
    match doc {
        Some(doc) => format!(
            "## Workbench\nThe draft pane shows '{}' ({} steps): {}",
            doc.identifier,
            doc.steps.len(),
            doc.description
        ),
        None => "## Workbench\nThe draft pane is empty.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::agent::doc::{global_fragments, AgentSet};

    #[test]
    fn the_workbench_agents_load_and_validate_together() {
        let (set, errors) = AgentSet::load(&builtin_sources(), &[]);
        assert!(errors.is_empty(), "{errors:?}");
        let fragments: Vec<&str> = global_fragments().into_keys().collect();
        let problems = set.validate(&fragments);
        assert!(problems.is_empty(), "{problems:?}");
        for name in [
            "front_desk",
            "plan_drafter",
            "plan_loader",
            "plan_editor",
            "tool_drafter",
            "agent_drafter",
        ] {
            assert!(set.get(name).is_some(), "{name}");
        }
    }

    #[test]
    fn each_way_into_the_workbench_starts_at_its_agent() {
        let doc: PlanDoc =
            serde_yaml::from_str("identifier: demo\nname: Demo\ndescription: d\nsteps: []\n")
                .unwrap();
        assert_eq!(starting_agent(&Start::FrontDesk, None), "front_desk");
        assert_eq!(starting_agent(&Start::Plan(None), None), "plan_drafter");
        assert_eq!(
            starting_agent(&Start::Plan(Some("demo".into())), Some(&doc)),
            "plan_editor"
        );
        assert_eq!(starting_agent(&Start::Agent(None), None), "agent_drafter");
        assert_eq!(
            starting_agent(&Start::Tool(Some("x".into())), None),
            "tool_drafter"
        );
    }

    #[test]
    fn the_current_draft_section_carries_the_yaml() {
        let doc: PlanDoc = serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
"#,
        )
        .unwrap();
        let section = current_draft(&Some(doc));
        assert!(section.starts_with("## Current draft"));
        assert!(section.contains("identifier: demo"));
        assert!(section.contains("do NOT call workbench__get_plan"));
        assert!(current_draft(&None).contains("none yet"));
    }

    #[test]
    fn each_agent_gets_its_context_sections() {
        let draft = Arc::new(Mutex::new(DraftState::new(None)));
        let hook = context_hook(draft);
        assert_eq!(
            hook("plan_drafter"),
            ["## Workbench\nThe draft pane is empty."]
        );
        assert!(hook("plan_editor")[0].starts_with("## Current draft"));
        assert!(hook("plan_loader").is_empty());
        assert!(hook("front_desk").is_empty());
        assert!(hook("tool_drafter")[0].starts_with("## Current draft"));
        assert!(hook("chat").is_empty());
    }
}
