//! Which project the server is allowed to load.
//!
//! On the command line, graph merges a global config layer
//! (`~/.config/graph/config.toml`) with a project one (`./.graph/config.toml`)
//! and searches both `./.graph/plans` and `~/.config/graph/plans`. That is
//! right for a CLI: the human chose the directory and typed the command, so
//! the working directory *is* an expression of intent.
//!
//! It is wrong for a server. An MCP client launches `graph mcp serve` with a
//! working directory of the client's choosing — commonly `/`, sometimes
//! whatever the user last had open — and the user never said anything about
//! that directory. Loading its `.graph/` anyway would mean an arbitrary
//! checkout could contribute:
//!
//! - `[mcp.<name>] command = …`, a child process graph spawns;
//! - `./.graph/tools/*.yaml`, user tools that shell out (`user_tools.rs`
//!   spawns `tokio::process::Command`);
//! - prompts, providers, and model routing.
//!
//! So the project layer is **opt-in**: `--dir` is the only thing that turns
//! it on. Without it the server runs on the global config alone, which is
//! the layer the user configured for themselves, independent of where any
//! client happens to start the process.
//!
//! `--dir .` is the one-token way to say "this directory, I mean it".

use anyhow::Result;
use graph_config::Config;
use std::path::Path;
use std::sync::OnceLock;

/// Whether the project layer was explicitly requested. Set once, before the
/// transport opens; every `Runtime` the server builds reads it.
static PINNED: OnceLock<bool> = OnceLock::new();

pub fn set_pinned(pinned: bool) {
    let _ = PINNED.set(pinned);
}

pub fn is_pinned() -> bool {
    *PINNED.get().unwrap_or(&false)
}

/// The config this server may use.
///
/// Pinned: the ordinary layered load, relative to the `--dir` we changed to.
/// Unpinned: the global file only, with every relative plan and tool path
/// dropped — because those resolve against the client's working directory,
/// which is exactly what must not be trusted here.
pub fn config() -> Result<Config> {
    if is_pinned() {
        return Ok(graph_config::load()?.config);
    }
    let mut config = graph_config::load_from(&[graph_config::global_config_path()])?.config;
    config
        .plans
        .paths
        .retain(|path| is_absolute(path.as_path()));
    config
        .tools
        .paths
        .retain(|path| is_absolute(path.as_path()));
    Ok(config)
}

/// Absolute *after* `~` expansion — `~/.config/graph/plans` is anchored to
/// the user's home and is fine; `./.graph/plans` is not.
fn is_absolute(path: &Path) -> bool {
    graph_config::expand_tilde(path).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_the_ones_that_follow_the_client_around() {
        // The distinction the unpinned filter turns on: a tilde path is
        // anchored to the user, a dot path is anchored to whatever directory
        // the client chose to launch us in.
        assert!(is_absolute(Path::new("~/.config/graph/plans")));
        assert!(is_absolute(Path::new("/etc/graph/plans")));
        assert!(!is_absolute(Path::new("./.graph/plans")));
        assert!(!is_absolute(Path::new(".graph/plans")));
    }
}
