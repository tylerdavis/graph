//! Layered config loading and post-processing.

use crate::model::Config;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where a config layer came from, for `graph config path`.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// Files that contributed, in application order (earlier is overridden by later).
    pub sources: Vec<PathBuf>,
}

pub fn global_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("graph")
        .join("config.toml")
}

pub fn project_config_path() -> PathBuf {
    PathBuf::from("./.graph/config.toml")
}

/// Load and merge the global and project config files.
///
/// Missing files are skipped; `~` in path values is expanded; `${VAR}`
/// references in string values are resolved from the environment.
pub fn load() -> Result<LoadedConfig> {
    load_from(&[global_config_path(), project_config_path()])
}

pub fn load_from(paths: &[PathBuf]) -> Result<LoadedConfig> {
    let mut merged = toml::Table::new();
    let mut sources = Vec::new();

    for path in paths {
        let path = expand_tilde(path);
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let table: toml::Table = raw
            .parse()
            .with_context(|| format!("parsing config file {}", path.display()))?;
        merge_tables(&mut merged, table);
        sources.push(path);
    }

    let expanded = expand_env_in_value(toml::Value::Table(merged))
        .context("expanding ${VAR} references in config")?;
    let config: Config = expanded
        .try_into()
        .context("config does not match the expected schema")?;

    for (name, server) in &config.mcp {
        server.validate(name).map_err(anyhow::Error::msg)?;
    }

    Ok(LoadedConfig { config, sources })
}

/// Deep-merge `overlay` into `base`: tables merge recursively, everything
/// else (including arrays) is replaced wholesale.
fn merge_tables(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_child)), toml::Value::Table(overlay_child)) => {
                merge_tables(base_child, overlay_child);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Resolve `${VAR}` references in every string value. Unset variables are an
/// error so misconfigured secrets fail loudly instead of sending "".
fn expand_env_in_value(value: toml::Value) -> Result<toml::Value> {
    Ok(match value {
        toml::Value::String(s) => toml::Value::String(expand_env(&s)?),
        toml::Value::Array(items) => toml::Value::Array(
            items
                .into_iter()
                .map(expand_env_in_value)
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(table) => toml::Value::Table(
            table
                .into_iter()
                .map(|(k, v)| Ok((k, expand_env_in_value(v)?)))
                .collect::<Result<_>>()?,
        ),
        other => other,
    })
}

fn expand_env(input: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .with_context(|| format!("unterminated ${{...}} in config value: {input:?}"))?;
        let var = &after[..end];
        let value = std::env::var(var)
            .with_context(|| format!("environment variable {var} referenced in config is not set"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderKind;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn merges_layers_with_project_overriding_global() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.toml",
            r#"
            [settings]
            history_limit = 10

            [models.default]
            provider = "anthropic"
            model = "claude-sonnet-5"

            [providers.anthropic]
            type = "anthropic"
            "#,
        );
        let project = write(
            dir.path(),
            "project.toml",
            r#"
            [settings]
            history_limit = 50

            [models.planner]
            provider = "anthropic"
            model = "claude-fable-5"
            "#,
        );

        let loaded = load_from(&[global, project]).unwrap();
        assert_eq!(loaded.config.settings.history_limit, 50);
        // Non-overridden defaults from the global layer survive the merge.
        assert_eq!(
            loaded.config.models.default.as_ref().unwrap().model,
            "claude-sonnet-5"
        );
        assert_eq!(
            loaded.config.models.planner.as_ref().unwrap().model,
            "claude-fable-5"
        );
        assert_eq!(loaded.sources.len(), 2);
    }

    #[test]
    fn expands_env_vars_and_fails_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GRAPH_TEST_KEY", "sk-123");
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [providers.anthropic]
            type = "anthropic"
            api_key = "${GRAPH_TEST_KEY}"
            "#,
        );
        let loaded = load_from(&[path.clone()]).unwrap();
        let provider = &loaded.config.providers["anthropic"];
        assert_eq!(provider.kind, ProviderKind::Anthropic);
        assert_eq!(provider.api_key.as_deref(), Some("sk-123"));

        let missing = write(
            dir.path(),
            "missing.toml",
            r#"
            [providers.openai]
            type = "openai"
            api_key = "${GRAPH_TEST_KEY_DOES_NOT_EXIST}"
            "#,
        );
        let err = load_from(&[missing]).unwrap_err();
        assert!(format!("{err:#}").contains("GRAPH_TEST_KEY_DOES_NOT_EXIST"));
    }

    #[test]
    fn mcp_server_requires_exactly_one_transport() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [mcp.broken]
            args = ["run"]
            "#,
        );
        let err = load_from(&[path]).unwrap_err();
        assert!(err.to_string().contains("broken"));
    }

    #[test]
    fn missing_files_yield_default_config() {
        let loaded = load_from(&[PathBuf::from("/nonexistent/config.toml")]).unwrap();
        assert!(loaded.sources.is_empty());
        assert_eq!(loaded.config.settings.max_agent_iterations, 15);
        assert_eq!(loaded.config.settings.planning_attempts, 2);
    }
}
