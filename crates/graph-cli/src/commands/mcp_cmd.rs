//! `graph mcp` — list/tools/test/probe.

use crate::cli::McpCommand;
use anyhow::{bail, Result};
use graph_mcp::McpManager;

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
    for def in defs {
        let annotation = match def.read_only {
            Some(true) => " [read-only]",
            _ => "",
        };
        let description = def.description.lines().next().unwrap_or_default();
        println!("{}{annotation}\n    {description}", def.name);
    }
    Ok(())
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
