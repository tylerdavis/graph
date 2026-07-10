//! `graph mcp` — list/tools/test/probe.

use crate::cli::McpCommand;
use anyhow::{bail, Result};
use graph_core::ToolDef;
use graph_mcp::{McpManager, NAMESPACE_SEPARATOR};
use std::io::IsTerminal;

pub async fn run(command: McpCommand) -> Result<()> {
    let loaded = graph_config::load()?;
    let manager = McpManager::new(loaded.config.mcp.clone());

    let result = match command {
        McpCommand::List => list(&loaded.config),
        McpCommand::Tools { server } => tools(&manager, server).await,
        McpCommand::Test { server } => test(&manager, &server).await,
        McpCommand::Probe { .. } => bail!("probe lands with the shape cache (phase 4)"),
    };
    manager.shutdown().await;
    result
}

fn list(config: &graph_config::Config) -> Result<()> {
    if config.mcp.is_empty() {
        println!("no MCP servers configured — add [mcp.<name>] sections to your config");
        return Ok(());
    }
    for (name, server) in &config.mcp {
        let transport = match (&server.command, &server.url) {
            (Some(command), _) => format!("stdio: {} {}", command, server.args.join(" ")),
            (_, Some(url)) => format!("http: {url}"),
            _ => "invalid".to_string(),
        };
        println!("{name}\t{transport}");
    }
    Ok(())
}

async fn tools(manager: &McpManager, server: Option<String>) -> Result<()> {
    let defs = match &server {
        Some(name) => manager.connect(name).await?,
        None => {
            use graph_core::ToolRegistry;
            manager.tools().await?
        }
    };
    if defs.is_empty() {
        println!("no tools exposed");
        return Ok(());
    }
    print!(
        "{}",
        render_tool_listing(&defs, std::io::stdout().is_terminal())
    );
    Ok(())
}

/// Group namespaced defs by server, one section per server: a header with
/// the tool count, then aligned bare names with one-line descriptions.
fn render_tool_listing(defs: &[ToolDef], color: bool) -> String {
    let bold = |s: &str| {
        if color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let dim = |s: &str| {
        if color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    // Bare name plus read-only marker — the padded left column.
    let label = |def: &ToolDef| {
        let bare = def
            .name
            .split_once(NAMESPACE_SEPARATOR)
            .map_or(def.name.as_str(), |(_, bare)| bare);
        match def.read_only {
            Some(true) => format!("{bare} [read-only]"),
            _ => bare.to_string(),
        }
    };

    // Group by namespace prefix, preserving discovery order.
    let mut groups: Vec<(&str, Vec<&ToolDef>)> = Vec::new();
    for def in defs {
        let server = def
            .name
            .split_once(NAMESPACE_SEPARATOR)
            .map_or("(unnamespaced)", |(server, _)| server);
        match groups.iter_mut().find(|(name, _)| *name == server) {
            Some((_, tools)) => tools.push(def),
            None => groups.push((server, vec![def])),
        }
    }

    let mut out = String::new();
    for (i, (server, tools)) in groups.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let noun = if tools.len() == 1 { "tool" } else { "tools" };
        out.push_str(&format!(
            "{} {}\n",
            bold(server),
            dim(&format!("— {} {noun}", tools.len()))
        ));
        let width = tools
            .iter()
            .map(|def| label(def).chars().count())
            .max()
            .unwrap_or(0);
        for def in tools {
            let description = def.description.lines().next().unwrap_or_default().trim();
            let padded = format!("  {:<width$}", label(def));
            if description.is_empty() {
                out.push_str(padded.trim_end());
            } else {
                out.push_str(&format!("{padded}  {}", dim(description)));
            }
            out.push('\n');
        }
    }
    out
}

async fn test(manager: &McpManager, server: &str) -> Result<()> {
    let started = std::time::Instant::now();
    let defs = manager.connect(server).await?;
    let elapsed = started.elapsed();
    let with_output_schema = defs.iter().filter(|d| d.output_schema.is_some()).count();
    println!(
        "ok: '{server}' initialized in {elapsed:.2?} — {} tools ({with_output_schema} declare output schemas)",
        defs.len(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, description: &str, read_only: Option<bool>) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            output_example: None,
            read_only,
        }
    }

    #[test]
    fn groups_by_server_and_aligns_names() {
        let defs = vec![
            def("everything__echo", "Echoes back the input.", None),
            def(
                "everything__get-sum",
                "Adds two numbers.\nSecond line.",
                Some(true),
            ),
            def("linear__list_issues", "List issues.", Some(true)),
        ];
        let rendered = render_tool_listing(&defs, false);
        assert_eq!(
            rendered,
            "everything — 2 tools\n\
             \x20 echo                 Echoes back the input.\n\
             \x20 get-sum [read-only]  Adds two numbers.\n\
             \n\
             linear — 1 tool\n\
             \x20 list_issues [read-only]  List issues.\n"
        );
    }

    #[test]
    fn empty_description_leaves_no_trailing_spaces() {
        let defs = vec![
            def("s__long_tool_name", "Described.", None),
            def("s__short", "", None),
        ];
        let rendered = render_tool_listing(&defs, false);
        assert!(rendered.contains("\n  short\n"), "{rendered:?}");
    }

    #[test]
    fn color_mode_bolds_headers_and_dims_descriptions() {
        let defs = vec![def("s__t", "Desc.", None)];
        let rendered = render_tool_listing(&defs, true);
        assert!(rendered.contains("\x1b[1ms\x1b[0m"), "{rendered:?}");
        assert!(rendered.contains("\x1b[2mDesc.\x1b[0m"), "{rendered:?}");
    }
}
