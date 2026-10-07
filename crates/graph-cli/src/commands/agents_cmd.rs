use crate::cli::AgentsCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{bail, Result};
use graph_core::agent::doc::{agent_dirs, global_fragments, load_agent_file, AgentDoc, AgentSet};
use graph_core::format::{migrate_file, stamp, Kind};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn run(command: AgentsCommand) -> Result<()> {
    let dirs = agent_dirs();
    match command {
        AgentsCommand::List { json } => report(list(&dirs), json),
        AgentsCommand::Show { name, json } => report(show(&dirs, &name)?, json),
        AgentsCommand::Validate { path, json } => report(validate(&dirs, path.as_deref())?, json),
        AgentsCommand::Migrate { path, json } => report(migrate(&path)?, json),
    }
}

fn subagent_io(doc: &AgentDoc) -> &'static str {
    match (&doc.input_schema, &doc.output_schema) {
        (None, None) => "{prompt} -> {result}",
        (Some(_), None) => "typed input -> {result}",
        (None, Some(_)) => "{prompt} -> typed output",
        (Some(_), Some(_)) => "typed input -> typed output",
    }
}

fn source(doc: &AgentDoc, workbench: bool) -> String {
    if workbench {
        "built-in, workbench only (graph wb)".to_string()
    } else {
        doc.source.describe()
    }
}

fn runnable_and_workbench(dirs: &[PathBuf]) -> (Vec<(AgentDoc, bool)>, Vec<String>) {
    let (set, errors) = AgentSet::load(graph_core::agent::doc::BUILTINS, dirs);
    let mut agents: Vec<(AgentDoc, bool)> = set.iter().map(|doc| (doc, false)).collect();
    for doc in crate::workbench::agents::workbench_only() {
        if !agents.iter().any(|(known, _)| known.name == doc.name) {
            agents.push((doc, true));
        }
    }
    agents.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
    (agents, errors)
}

fn summary(doc: &AgentDoc, workbench: bool) -> Value {
    json!({
        "name": doc.name,
        "description": doc.description,
        "source": source(doc, workbench),
        "workbenchOnly": workbench,
        "model": doc.model,
        "subagents": doc.subagents,
        "handoffs": doc.handoffs,
        "subagentIo": subagent_io(doc),
    })
}

pub(crate) fn list(dirs: &[PathBuf]) -> Outcome {
    let (listed, errors) = runnable_and_workbench(dirs);
    let agents: Vec<Value> = listed
        .iter()
        .map(|(doc, workbench)| summary(doc, *workbench))
        .collect();
    let text: String = listed
        .iter()
        .map(|(doc, workbench)| {
            format!(
                "{}\t{}\t{}\n",
                doc.name,
                source(doc, *workbench),
                doc.description
            )
        })
        .collect();
    let outcome = Outcome::raw(
        text,
        json!({ "agents": agents, "count": agents.len(), "loadErrors": errors }),
    );
    if errors.is_empty() {
        outcome
    } else {
        outcome.with_note(format!(
            "{} agent file(s) failed to load — run `graph agents validate`",
            errors.len()
        ))
    }
}

pub(crate) fn show(dirs: &[PathBuf], name: &str) -> Result<Outcome> {
    let (listed, _) = runnable_and_workbench(dirs);
    let Some((doc, workbench)) = listed.iter().find(|(doc, _)| doc.name == name).cloned() else {
        let known: Vec<String> = listed.iter().map(|(d, _)| d.name.clone()).collect();
        bail!("unknown agent '{name}' (defined: {})", known.join(", "));
    };
    let mut value = serde_yaml::to_value(&doc)?;
    stamp(Kind::Agent, &mut value);
    let yaml = serde_yaml::to_string(&value)?;
    let body = json!({
        "agent": serde_json::to_value(&doc)?,
        "source": doc.source.describe(),
        "yaml": yaml,
    });
    Ok(Outcome::raw(yaml, body).with_note(format!("source: {}", source(&doc, workbench))))
}

pub(crate) fn validate(dirs: &[PathBuf], path: Option<&Path>) -> Result<Outcome> {
    let (mut set, mut problems) =
        AgentSet::load(&crate::workbench::agents::builtin_sources(), dirs);
    let mut checked = "all agents".to_string();
    if let Some(path) = path {
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        checked = path.display().to_string();
        match load_agent_file(path) {
            Ok(doc) => {
                let name = doc.name.clone();
                set = AgentSet::layered(set.iter().collect(), vec![vec![doc]]);
                problems = set.agent_problems(&name, &fragment_names());
            }
            Err(error) => problems = vec![error],
        }
    } else {
        problems.extend(set.validate(&fragment_names()));
    }
    if problems.is_empty() {
        return Ok(Outcome::ok(
            json!({ "ok": true, "checked": checked, "problems": problems }),
        ));
    }
    Ok(Outcome::rejected(json!({
        "ok": false,
        "checked": checked,
        "error": format!("{checked} has problems"),
        "problems": problems,
    })))
}

fn fragment_names() -> Vec<&'static str> {
    global_fragments().into_keys().collect()
}

fn migrate(path: &Path) -> Result<Outcome> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let check = |value: &serde_yaml::Value| -> Result<(), String> {
        let yaml = serde_yaml::to_string(value).map_err(|e| e.to_string())?;
        graph_core::agent::doc::parse_agent_source(&yaml).map(|_| ())
    };
    let migrated = migrate_file(Kind::Agent, path, &check).map_err(anyhow::Error::msg)?;
    Ok(crate::commands::plan_cmd::migrated_outcome(migrated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::agent::doc::AgentSource;

    fn write_agent(dir: &Path, file: &str, body: &str) -> PathBuf {
        let path = dir.join(file);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn list_includes_the_builtin_chat_agent_and_project_files() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "search_bot.yaml",
            "name: search_bot\ndescription: searches\nmodel: chat\ntools: []\ninput_schema: {type: object, required: [query], properties: {query: {type: string, description: what to find}}}\noutput_schema: {type: object, properties: {hits: {type: array}}}\nsystem_prompt: hi\n",
        );
        let outcome = list(&[dir.path().to_path_buf()]);
        let names: Vec<&str> = outcome.body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "agent_drafter",
                "chat",
                "front_desk",
                "plan_drafter",
                "plan_editor",
                "plan_loader",
                "search_bot",
                "tool_drafter"
            ]
        );
        assert_eq!(outcome.body["agents"][0]["source"], "built-in");
        let by_name = |name: &str| {
            outcome.body["agents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|agent| agent["name"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("plan_drafter")["workbenchOnly"], true);
        assert_eq!(by_name("tool_drafter")["workbenchOnly"], false);
        assert_eq!(by_name("chat")["workbenchOnly"], false);
        assert_eq!(
            outcome.body["agents"][6]["subagentIo"],
            "typed input -> typed output"
        );
        assert_eq!(
            outcome.body["agents"][7]["subagentIo"],
            "{prompt} -> {result}"
        );
    }

    #[test]
    fn show_prints_stamped_yaml_from_the_overriding_file() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "chat.yaml",
            "name: chat\ndescription: mine\nmodel: chat\ntools: []\nsystem_prompt: hi\n",
        );
        let outcome = show(&[dir.path().to_path_buf()], "chat").unwrap();
        let yaml = outcome.raw.unwrap();
        assert!(yaml.starts_with("version: 1\n"), "{yaml}");
        assert!(yaml.contains("description: mine"));
        assert!(outcome.body["source"]
            .as_str()
            .unwrap()
            .ends_with("chat.yaml"));
        assert!(show(&[], "nope").is_err());
    }

    #[test]
    fn validate_reports_one_files_problems_against_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_agent(
            dir.path(),
            "bad.yaml",
            "name: bad\ndescription: d\nmodel: chat\ntools: [map]\nsubagents: [ghost]\nsystem_prompt: hi\n",
        );
        let outcome = validate(&[], Some(&path)).unwrap();
        assert!(outcome.rejected);
        let problems = outcome.body["problems"].to_string();
        assert!(problems.contains("'map' is a plan step"), "{problems}");
        assert!(problems.contains("subagent 'ghost'"), "{problems}");
        let clean = validate(&[], None).unwrap();
        assert!(!clean.rejected, "{}", clean.body);
    }

    #[test]
    fn migrate_stamps_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_agent(
            dir.path(),
            "a.yaml",
            "name: a\ndescription: d\nmodel: chat\ntools: []\nsystem_prompt: hi\n",
        );
        let outcome = migrate(&path).unwrap();
        assert_eq!(outcome.body["to"], 1);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("version: 1\n"));
    }

    #[test]
    fn builtin_sources_are_reported_as_built_in() {
        let (set, _) = AgentSet::load(&crate::workbench::agents::builtin_sources(), &[]);
        assert_eq!(set.get("chat").unwrap().source, AgentSource::Builtin);
    }
}
