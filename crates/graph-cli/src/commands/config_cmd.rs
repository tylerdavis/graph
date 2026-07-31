//! `graph config` — show/init/path.

use crate::cli::ConfigCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{bail, Context, Result};
use serde_json::json;

const STARTER_CONFIG: &str = r#"# graph configuration
# Values in ${VAR} form are read from the environment at load time.

[settings]
# data_dir = "~/.local/share/graph"
# max_agent_iterations = 15
# planning_attempts = 2

# [storage]
# backend = "file"      # default: plain files under data_dir
# backend = "memory"    # ephemeral (CI jobs); or set GRAPH_STORAGE=memory

[providers.anthropic]
type = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"

# [providers.local]
# type = "openai_compat"
# base_url = "http://localhost:11434/v1"

# [providers.bedrock]
# type = "bedrock"
# region = "us-east-1"

[models]
default = { provider = "anthropic", model = "claude-sonnet-5" }
# planner = { provider = "anthropic", model = "claude-fable-5", temperature = 0.0 }
# solver  = { provider = "anthropic", model = "claude-haiku-4-5", temperature = 0.4 }

# [mcp.github]
# command = "docker"
# args = ["run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN", "ghcr.io/github/github-mcp-server"]
# env = { GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}" }

# [mcp.linear]
# url = "https://mcp.linear.app/mcp"
# headers = { Authorization = "Bearer ${LINEAR_API_KEY}" }

[user]
# name = "Your Name"
# context = "Role, primary repos, teams — injected into prompts."

# System prompts, written out so they are visible and editable. Each field
# replaces the built-in text wholesale; delete a field to fall back to the
# built-in default (which may improve across releases).
"#;

/// The starter file: the commented skeleton plus a `[prompts]` section
/// carrying the built-in system prompts, serialized from the real
/// constants so the starter can never drift from the shipped defaults.
fn starter_config() -> Result<String> {
    let mut prompts = toml::Table::new();
    prompts.insert(
        "chat".into(),
        toml::Value::String(graph_core::prompts::DEFAULT_CHAT_PROMPT.into()),
    );
    prompts.insert(
        "workbench".into(),
        toml::Value::String(crate::workbench::WORKBENCH_SYSTEM_PROMPT.into()),
    );
    let mut root = toml::Table::new();
    root.insert("prompts".into(), toml::Value::Table(prompts));
    let rendered = toml::to_string_pretty(&root).context("serializing default prompts")?;
    Ok(format!("{STARTER_CONFIG}{rendered}"))
}

pub fn run(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show { json } => report(show()?, json),
        ConfigCommand::Path { json } => report(path(), json),
        ConfigCommand::Init { global, force, .. } => report(init(global, force)?, false),
    }
}

/// The effective config. The text rendering is TOML — the format you would
/// paste back into a config file — while `--json` gives the same values in
/// the shape a program addresses.
fn show() -> Result<Outcome> {
    let loaded = graph_config::load()?;
    let rendered = toml::to_string_pretty(&loaded.config)?;
    let sources: Vec<String> = loaded
        .sources
        .iter()
        .map(|source| source.display().to_string())
        .collect();
    let body = json!({
        "config": serde_json::to_value(&loaded.config)?,
        "sources": sources,
    });
    if loaded.sources.is_empty() {
        eprintln!("# no config files found — showing defaults (run `graph config init`)");
    } else {
        for source in &loaded.sources {
            eprintln!("# merged from {}", source.display());
        }
    }
    Ok(Outcome::raw(rendered, body))
}

fn path() -> Outcome {
    let mut text = String::new();
    let mut files = Vec::new();
    for candidate in [
        graph_config::global_config_path(),
        graph_config::project_config_path(),
    ] {
        let expanded = graph_config::expand_tilde(&candidate);
        let exists = expanded.exists();
        let marker = if exists { "exists" } else { "missing" };
        text.push_str(&format!("{}\t{marker}\n", expanded.display()));
        files.push(json!({"path": expanded.display().to_string(), "exists": exists}));
    }
    Outcome::raw(text, json!({"files": files, "count": files.len()}))
}

fn init(global: bool, force: bool) -> Result<Outcome> {
    let target = if global {
        graph_config::global_config_path()
    } else {
        graph_config::project_config_path()
    };
    let target = graph_config::expand_tilde(&target);
    if target.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&target, starter_config()?)
        .with_context(|| format!("writing {}", target.display()))?;
    Ok(Outcome::ok(
        json!({"ok": true, "savedTo": target.display().to_string()}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_parses_and_carries_the_builtin_prompts() {
        let starter = starter_config().unwrap();
        // deny_unknown_fields on the model makes this catch skeleton drift.
        let config: graph_config::Config = toml::from_str(&starter).unwrap();
        assert_eq!(
            config.prompts.chat.as_deref(),
            Some(graph_core::prompts::DEFAULT_CHAT_PROMPT)
        );
        assert_eq!(
            config.prompts.workbench.as_deref(),
            Some(crate::workbench::WORKBENCH_SYSTEM_PROMPT)
        );
    }
}
