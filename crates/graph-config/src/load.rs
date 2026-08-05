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

/// `~/.config/graph/config.toml` on every platform — CLI convention rather
/// than the OS-native config dir (`~/Library/Application Support` on macOS).
pub fn global_config_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".config"))
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
///
/// A `${VAR}` whose variable is unset is a **deferred** error inside
/// `[providers.*]` and `[mcp.*]`: the value keeps its literal `${VAR}` text,
/// the entry's `missing_env` records the variable, and the error surfaces —
/// naming the variable — when that provider or server is actually used. This
/// keeps everything that never touches the entry (plan authoring, listing,
/// serving) working without the secret, and it is why `graph mcp serve` can
/// start and *explain* a missing key instead of dying before the handshake.
/// Anywhere else a missing variable still fails the load loudly — a path or
/// prompt carrying literal `${VAR}` text would be silently wrong everywhere.
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

    let mut missing = Vec::new();
    let expanded = expand_env_in_value(toml::Value::Table(merged), "", &mut missing)
        .context("expanding ${VAR} references in config")?;
    let mut config: Config = expanded
        .try_into()
        .context("config does not match the expected schema")?;
    distribute_missing_env(&mut config, missing)?;

    for (name, server) in &config.mcp {
        server.validate(name).map_err(anyhow::Error::msg)?;
    }

    for name in config.models.named.keys() {
        if crate::model::RESERVED_MODEL_NAMES.contains(&name.as_str()) {
            anyhow::bail!(
                "[models.named] entry '{name}' shadows a built-in role name; \
                 reserved names: {}",
                crate::model::RESERVED_MODEL_NAMES.join(", ")
            );
        }
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

/// Resolve `${VAR}` references in every string value. An unset variable
/// leaves the reference unexpanded and records it in `missing` under the
/// value's dotted path (`providers.anthropic.api_key`) — whether that is an
/// error, and when, is [`distribute_missing_env`]'s decision.
fn expand_env_in_value(
    value: toml::Value,
    path: &str,
    missing: &mut Vec<(String, String)>,
) -> Result<toml::Value> {
    let child = |path: &str, key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    Ok(match value {
        toml::Value::String(s) => toml::Value::String(expand_env(&s, path, missing)?),
        toml::Value::Array(items) => toml::Value::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    expand_env_in_value(item, &child(path, &index.to_string()), missing)
                })
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(table) => toml::Value::Table(
            table
                .into_iter()
                .map(|(k, v)| {
                    let expanded = expand_env_in_value(v, &child(path, &k), missing)?;
                    Ok((k, expanded))
                })
                .collect::<Result<_>>()?,
        ),
        other => other,
    })
}

fn expand_env(input: &str, path: &str, missing: &mut Vec<(String, String)>) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .with_context(|| format!("unterminated ${{...}} in config value: {input:?}"))?;
        let var = &after[..end];
        match std::env::var(var) {
            Ok(value) => out.push_str(&value),
            Err(_) => {
                missing.push((path.to_string(), var.to_string()));
                out.push_str(&rest[start..start + 2 + end + 1]);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Sort the unset `${VAR}` references onto whoever must report them.
///
/// `[providers.<name>]` and `[mcp.<name>]` entries own their references:
/// they stay loadable and carry the record in `missing_env`, erroring only
/// when used. A missing variable anywhere else fails the load, exactly as
/// every missing variable used to — secrets must never silently become
/// empty strings or literal `${VAR}` text.
fn distribute_missing_env(config: &mut Config, missing: Vec<(String, String)>) -> Result<()> {
    for (path, var) in missing {
        let mut parts = path.splitn(3, '.');
        let entry = match (parts.next(), parts.next(), parts.next()) {
            (Some("providers"), Some(name), Some(field)) => config
                .providers
                .get_mut(name)
                .map(|provider| (&mut provider.missing_env, field)),
            (Some("mcp"), Some(name), Some(field)) => config
                .mcp
                .get_mut(name)
                .map(|server| (&mut server.missing_env, field)),
            _ => None,
        };
        match entry {
            Some((missing_env, field)) => missing_env.push(crate::model::MissingEnv {
                field: field.to_string(),
                var,
            }),
            None => {
                anyhow::bail!("environment variable {var} referenced in config ({path}) is not set")
            }
        }
    }
    Ok(())
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
    fn named_models_parse_resolve_and_reject_role_shadowing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
[models]
default = { provider = "p", model = "m" }

[models.named.nano]
provider = "p"
model = "nano-model"
description = "fast and cheap"
"#,
        );
        let loaded = load_from(&[path]).unwrap();
        let models = &loaded.config.models;
        assert_eq!(models.named["nano"].model, "nano-model");
        assert_eq!(
            models.named["nano"].description.as_deref(),
            Some("fast and cheap")
        );
        assert_eq!(models.resolve_name("nano").unwrap().model, "nano-model");
        // Role names resolve with the default fallback; unknown names don't.
        assert_eq!(models.resolve_name("solver").unwrap().model, "m");
        assert!(models.resolve_name("bogus").is_none());

        let path = write(
            dir.path(),
            "bad.toml",
            r#"
[models.named.judge]
provider = "p"
model = "m"
"#,
        );
        let err = load_from(&[path]).unwrap_err().to_string();
        assert!(err.contains("shadows a built-in role name"), "{err}");
    }

    #[test]
    fn model_fallbacks_parse_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
[models.chat]
provider = "anthropic"
model = "claude-sonnet-5"
fallbacks = [
    { provider = "openai", model = "gpt-5" },
    { provider = "local", model = "llama", temperature = 0.1 },
]
"#,
        );
        let loaded = load_from(&[path]).unwrap();
        let chat = loaded.config.models.chat.as_ref().unwrap();
        assert_eq!(chat.fallbacks.len(), 2);
        assert_eq!(chat.fallbacks[0].provider, "openai");
        assert_eq!(chat.fallbacks[0].temperature, None);
        assert_eq!(chat.fallbacks[1].model, "llama");
        assert_eq!(chat.fallbacks[1].temperature, Some(0.1));

        // Serialize → parse round-trips, and entries without fallbacks
        // stay backward-compatible (field omitted, defaults to empty).
        let rendered = toml::to_string(&loaded.config).unwrap();
        let reparsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed.models.chat.unwrap().fallbacks.len(), 2);
        assert!(loaded
            .config
            .models
            .all_choices()
            .all(|c| c.provider == "anthropic"));
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
    fn expands_env_vars_when_set() {
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
        let loaded = load_from(std::slice::from_ref(&path)).unwrap();
        let provider = &loaded.config.providers["anthropic"];
        assert_eq!(provider.kind, ProviderKind::Anthropic);
        assert_eq!(provider.api_key.as_deref(), Some("sk-123"));
        assert!(provider.missing_env.is_empty());
    }

    #[test]
    fn a_missing_var_in_a_provider_defers_instead_of_failing_the_load() {
        // The whole config must stay loadable: plan authoring, listing, and
        // `graph mcp serve` all work without the secret. The provider itself
        // carries the record and errors when a model resolves to it.
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [providers.openai]
            type = "openai"
            api_key = "${GRAPH_TEST_KEY_DOES_NOT_EXIST}"
            "#,
        );
        let loaded = load_from(&[path]).unwrap();
        let provider = &loaded.config.providers["openai"];
        // The value keeps the literal reference — visible in `config show`,
        // and obviously wrong if it ever leaks past the missing_env check.
        assert_eq!(
            provider.api_key.as_deref(),
            Some("${GRAPH_TEST_KEY_DOES_NOT_EXIST}")
        );
        assert_eq!(
            provider.missing_env,
            vec![crate::model::MissingEnv {
                field: "api_key".into(),
                var: "GRAPH_TEST_KEY_DOES_NOT_EXIST".into(),
            }]
        );
    }

    #[test]
    fn a_missing_var_in_an_mcp_server_defers_with_its_field_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [mcp.linear]
            url = "https://mcp.linear.app/mcp"
            headers = { Authorization = "Bearer ${GRAPH_TEST_TOKEN_DOES_NOT_EXIST}" }
            "#,
        );
        let loaded = load_from(&[path]).unwrap();
        let server = &loaded.config.mcp["linear"];
        assert_eq!(
            server.missing_env,
            vec![crate::model::MissingEnv {
                field: "headers.Authorization".into(),
                var: "GRAPH_TEST_TOKEN_DOES_NOT_EXIST".into(),
            }]
        );
        // The describe helper is what the manager's refusal quotes.
        assert_eq!(
            crate::model::describe_missing_env("mcp.linear", &server.missing_env),
            "environment variable GRAPH_TEST_TOKEN_DOES_NOT_EXIST \
             (mcp.linear.headers.Authorization) is not set"
        );
    }

    #[test]
    fn a_missing_var_outside_providers_and_mcp_still_fails_the_load() {
        // A data_dir (or prompt, or plan path) carrying literal `${VAR}` text
        // would be silently wrong everywhere, so only the two entry kinds
        // that can report the problem at use time get the deferral.
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [settings]
            data_dir = "${GRAPH_TEST_DIR_DOES_NOT_EXIST}"
            "#,
        );
        let err = load_from(&[path]).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("GRAPH_TEST_DIR_DOES_NOT_EXIST")
                && rendered.contains("settings.data_dir"),
            "{rendered}"
        );
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

    #[test]
    fn prompt_overrides_parse_and_layer() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.toml",
            r#"
            [prompts]
            chat = "global chat prompt"
            workbench = "global workbench addendum"
            "#,
        );
        let project = write(
            dir.path(),
            "project.toml",
            r#"
            [prompts]
            chat = "project chat prompt"
            "#,
        );
        let loaded = load_from(&[global, project]).unwrap();
        assert_eq!(
            loaded.config.prompts.chat.as_deref(),
            Some("project chat prompt")
        );
        // The workbench override from the global layer survives the merge.
        assert_eq!(
            loaded.config.prompts.workbench.as_deref(),
            Some("global workbench addendum")
        );
        // And both default to unset.
        let empty = load_from(&[]).unwrap();
        assert!(empty.config.prompts.chat.is_none());
        assert!(empty.config.prompts.workbench.is_none());
    }

    #[test]
    fn tool_packs_parse_and_default_paths_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [tools]
            packs = ["github"]
            "#,
        );
        let loaded = load_from(&[path]).unwrap();
        assert_eq!(loaded.config.tools.packs, vec!["github".to_string()]);
        // Setting packs alone must not wipe the default search paths.
        assert_eq!(loaded.config.tools.paths.len(), 2);
    }
}
