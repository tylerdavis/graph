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

fn summary(doc: &AgentDoc) -> Value {
    json!({
        "name": doc.name,
        "description": doc.description,
        "source": doc.source.describe(),
        "model": doc.model,
        "subagents": doc.subagents,
        "handoffs": doc.handoffs,
        "subagentIo": subagent_io(doc),
    })
}

pub(crate) fn list(dirs: &[PathBuf]) -> Outcome {
    let (set, errors) = AgentSet::load(&crate::workbench::agents::builtin_sources(), dirs);
    let agents: Vec<Value> = set.iter().map(summary).collect();
    let text: String = set
        .iter()
        .map(|doc| {
            format!(
                "{}\t{}\t{}\n",
                doc.name,
                doc.source.describe(),
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
    let (set, _) = AgentSet::load(&crate::workbench::agents::builtin_sources(), dirs);
    let Some(doc) = set.get(name) else {
        let known: Vec<&str> = set.iter().map(|d| d.name.as_str()).collect();
        bail!("unknown agent '{name}' (defined: {})", known.join(", "));
    };
    let mut value = serde_yaml::to_value(doc)?;
    stamp(Kind::Agent, &mut value);
    let yaml = serde_yaml::to_string(&value)?;
    let body = json!({
        "agent": serde_json::to_value(doc)?,
        "source": doc.source.describe(),
        "yaml": yaml,
    });
    Ok(Outcome::raw(yaml, body).with_note(format!("source: {}", doc.source.describe())))
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
                set = AgentSet::layered(set.iter().cloned().collect(), vec![vec![doc]]);
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
            "name: search_bot\ndescription: searches\nmodel: chat\ntools: []\nsystem_prompt: hi\n",
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
                "chat",
                "orchestrator",
                "plan_author",
                "plan_editor",
                "plan_loader",
                "plan_refiner",
                "plan_verifier",
                "search_bot"
            ]
        );
        assert_eq!(outcome.body["agents"][0]["source"], "built-in");
        assert_eq!(
            outcome.body["agents"][7]["subagentIo"],
            "{prompt} -> {result}"
        );
        assert_eq!(
            outcome.body["agents"][6]["subagentIo"],
            "typed input -> typed output"
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
