//! `graph config` — show/init/path/check/migrate.

use crate::cli::ConfigCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{bail, Context, Result};
use graph_config::{CONFIG_FORMAT, FORMAT_KEY};
use serde_json::json;
use std::path::PathBuf;

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

# Token prices, in USD per million tokens, keyed by model id. Without them a
# run still reports its token counts — it just omits the dollar figure rather
# than guessing one. Published prices change: check them before relying on
# the number, and keep these entries current.
# [pricing."claude-sonnet-5"]
# input = 3.00
# output = 15.00
# cache_write = 3.75   # optional; defaults to input x 1.25
# cache_read = 0.30    # optional; defaults to input x 0.10

# [pricing."claude-haiku-4-5"]
# input = 1.00
# output = 5.00

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

/// The starter file: the format stamp, the commented skeleton, then a
/// `[prompts]` section carrying the built-in system prompts, serialized
/// from the real constants so the starter can never drift from the shipped
/// defaults.
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
    Ok(format!(
        "{FORMAT_KEY} = {CONFIG_FORMAT}\n\n{STARTER_CONFIG}{rendered}"
    ))
}

pub fn run(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show { json } => report(show()?, json),
        ConfigCommand::Path { json } => report(path(), json),
        ConfigCommand::Init { global, force, .. } => report(init(global, force)?, false),
        ConfigCommand::Check { json } => report(check(), json),
        ConfigCommand::Migrate { global, json } => report(migrate(global)?, json),
    }
}

/// The effective config. The text rendering is TOML — the format you would
/// paste back into a config file — while `--json` gives the same values in
/// the shape a program addresses.
fn show() -> Result<Outcome> {
    let loaded = crate::runtime::load_config()?;
    let rendered = format!(
        "{FORMAT_KEY} = {CONFIG_FORMAT}\n\n{}",
        toml::to_string_pretty(&loaded.config)?
    );
    let sources: Vec<String> = loaded
        .sources
        .iter()
        .map(|source| source.display().to_string())
        .collect();
    let body = json!({
        "formatVersion": CONFIG_FORMAT,
        "config": serde_json::to_value(&loaded.config)?,
        "sources": sources,
        "layers": loaded.layers.iter().map(layer_json).collect::<Vec<_>>(),
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

fn layer_json(layer: &graph_config::LayerInfo) -> serde_json::Value {
    json!({
        "path": layer.path.display().to_string(),
        "formatVersion": layer.format_version,
        "declared": layer.declared,
        "current": layer.format_version == CONFIG_FORMAT,
        "notes": layer.notes,
    })
}

fn candidates() -> [PathBuf; 2] {
    [
        graph_config::expand_tilde(&graph_config::global_config_path()),
        graph_config::expand_tilde(&graph_config::project_config_path()),
    ]
}

fn path() -> Outcome {
    let mut text = String::new();
    let mut files = Vec::new();
    for candidate in candidates() {
        let exists = candidate.exists();
        let format = exists
            .then(|| graph_config::inspect(&candidate).ok())
            .flatten();
        let marker = match (&format, exists) {
            (Some(info), _) => format!("exists\tformat {}", info.format_version),
            (None, true) => "exists".to_string(),
            (None, false) => "missing".to_string(),
        };
        text.push_str(&format!("{}\t{marker}\n", candidate.display()));
        files.push(json!({
            "path": candidate.display().to_string(),
            "exists": exists,
            "formatVersion": format.as_ref().map(|info| info.format_version),
        }));
    }
    Outcome::raw(text, json!({"files": files, "count": files.len()}))
}

/// Each file's format on its own terms, then whether the merged whole loads.
/// A file this binary cannot read, or a merged config that fails the schema,
/// is a rejection: the body still carries every file's verdict.
fn check() -> Outcome {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    let mut text = String::new();
    for candidate in candidates() {
        if !candidate.exists() {
            text.push_str(&format!("{}\tmissing\n", candidate.display()));
            files.push(json!({"path": candidate.display().to_string(), "exists": false}));
            continue;
        }
        match graph_config::inspect(&candidate) {
            Ok(info) => {
                let verdict = if let Some(problem) = graph_config::window_problem(
                    "config",
                    &candidate.display(),
                    info.format_version,
                    graph_config::CONFIG_FORMAT_OLDEST,
                    CONFIG_FORMAT,
                ) {
                    problems.push(problem);
                    if info.format_version > CONFIG_FORMAT {
                        "too new"
                    } else {
                        "too old"
                    }
                } else if info.format_version < CONFIG_FORMAT {
                    "migrate"
                } else if info.declared.is_none() {
                    "current (unstamped)"
                } else {
                    "current"
                };
                text.push_str(&format!(
                    "{}\tformat {}\t{verdict}\n",
                    candidate.display(),
                    info.format_version
                ));
                files.push(json!({
                    "path": candidate.display().to_string(),
                    "exists": true,
                    "formatVersion": info.format_version,
                    "declared": info.declared,
                    "readable": (graph_config::CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT).contains(&info.format_version),
                    "current": info.format_version == CONFIG_FORMAT,
                }));
            }
            Err(error) => {
                problems.push(error.clone());
                text.push_str(&format!("{}\tunreadable\n", candidate.display()));
                files.push(json!({
                    "path": candidate.display().to_string(),
                    "exists": true,
                    "error": error,
                }));
            }
        }
    }
    if problems.is_empty() {
        if let Err(error) = graph_config::load() {
            problems.push(format!("{error:#}"));
        }
    }
    let ok = problems.is_empty();
    let body = json!({
        "ok": ok,
        "formatVersion": CONFIG_FORMAT,
        "files": files,
        "problems": problems,
    });
    if ok {
        Outcome::raw(text, body)
    } else {
        let mut body = body;
        body["error"] = json!("config does not load");
        Outcome::rejected(body)
    }
}

fn migrate(global: bool) -> Result<Outcome> {
    let target = if global {
        graph_config::global_config_path()
    } else {
        graph_config::project_config_path()
    };
    let target = graph_config::expand_tilde(&target);
    if !target.exists() {
        bail!(
            "{} does not exist (run `graph config init{}` to create it)",
            target.display(),
            if global { " --global" } else { "" }
        );
    }
    let migrated = graph_config::migrate_file(&target).map_err(anyhow::Error::msg)?;
    let mut outcome = Outcome::ok(json!({
        "ok": true,
        "savedTo": migrated.path.display().to_string(),
        "from": migrated.from,
        "to": migrated.to,
        "changed": migrated.changed,
        "notes": migrated.notes,
    }));
    if !migrated.changed {
        outcome = outcome.with_note(format!("already at config format {}", migrated.to));
    } else if !migrated.notes.is_empty() {
        outcome = outcome.with_note(migrated.notes.join("; "));
    }
    Ok(outcome)
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, &starter).unwrap();
        // deny_unknown_fields on the model makes this catch skeleton drift.
        let loaded = graph_config::load_from(&[path]).unwrap();
        let config = loaded.config;
        assert_eq!(
            config.prompts.chat.as_deref(),
            Some(graph_core::prompts::DEFAULT_CHAT_PROMPT)
        );
        assert_eq!(
            config.prompts.workbench.as_deref(),
            Some(crate::workbench::WORKBENCH_SYSTEM_PROMPT)
        );
        assert_eq!(loaded.layers[0].declared, Some(CONFIG_FORMAT));
        assert!(starter.starts_with(&format!("{FORMAT_KEY} = {CONFIG_FORMAT}\n")));
    }
}
