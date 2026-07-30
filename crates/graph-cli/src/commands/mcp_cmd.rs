//! `graph mcp` — list/tools/test/probe.

use super::listing::{render_tool_listing, tool_listing_as_json};
use crate::cli::McpCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{bail, Result};
use graph_mcp::McpManager;
use serde_json::json;
use std::io::IsTerminal;

pub async fn run(command: McpCommand) -> Result<()> {
    let loaded = graph_config::load()?;
    let manager = McpManager::new(loaded.config.mcp.clone());

    let result = match command {
        McpCommand::List { json } => list(&loaded.config).map(|o| (o, json)),
        McpCommand::Tools { server, json } => tools(&manager, server).await.map(|o| (o, json)),
        McpCommand::Test { server, json } => test(&manager, &server).await.map(|o| (o, json)),
        McpCommand::Probe { .. } => bail!("probe lands with the shape cache (phase 4)"),
    };
    // Before reporting: a failure must still shut the children down, and
    // `report` can exit the process's unwind path.
    manager.shutdown().await;
    let (outcome, json) = result?;
    report(outcome, json)
}

fn list(config: &graph_config::Config) -> Result<Outcome> {
    let servers: Vec<_> = config
        .mcp
        .iter()
        .map(|(name, server)| {
            let (transport, target) = match (&server.command, &server.url) {
                (Some(command), _) => (
                    "stdio",
                    format!("{} {}", command, server.args.join(" "))
                        .trim_end()
                        .to_string(),
                ),
                (_, Some(url)) => ("http", url.clone()),
                _ => ("invalid", String::new()),
            };
            json!({"name": name, "transport": transport, "target": target})
        })
        .collect();
    let body = json!({"servers": servers, "count": servers.len()});
    if config.mcp.is_empty() {
        return Ok(Outcome::raw(String::new(), body)
            .with_note("no MCP servers configured — add [mcp.<name>] sections to your config"));
    }
    let text = servers
        .iter()
        .map(|s| {
            format!(
                "{}\t{}: {}\n",
                s["name"].as_str().unwrap_or_default(),
                s["transport"].as_str().unwrap_or_default(),
                s["target"].as_str().unwrap_or_default()
            )
        })
        .collect::<String>();
    Ok(Outcome::raw(text, body))
}

async fn tools(manager: &McpManager, server: Option<String>) -> Result<Outcome> {
    let defs = match &server {
        Some(name) => manager.connect(name).await?,
        None => {
            use graph_core::ToolRegistry;
            manager.tools().await?
        }
    };
    let body = tool_listing_as_json(&defs);
    if defs.is_empty() {
        return Ok(Outcome::raw(String::new(), body).with_note("no tools exposed"));
    }
    Ok(Outcome::raw(
        render_tool_listing(&defs, std::io::stdout().is_terminal()),
        body,
    ))
}

async fn test(manager: &McpManager, server: &str) -> Result<Outcome> {
    let started = std::time::Instant::now();
    let defs = manager.connect(server).await?;
    let elapsed = started.elapsed();
    let with_output_schema = defs.iter().filter(|d| d.output_schema.is_some()).count();
    let body = json!({
        "server": server,
        "ok": true,
        "tools": defs.len(),
        "declareOutputSchema": with_output_schema,
        "elapsedMs": elapsed.as_millis(),
    });
    let text = format!(
        "ok: '{server}' initialized in {elapsed:.2?} — {} tools \
         ({with_output_schema} declare output schemas)\n",
        defs.len(),
    );
    Ok(Outcome::raw(text, body))
}
