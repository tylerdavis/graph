//! `graph tools` — inspect the tool catalog.

use crate::cli::ToolsCommand;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::ToolRegistry;

pub async fn run(command: ToolsCommand) -> Result<()> {
    let runtime = Runtime::init()?;
    let store = runtime.store()?;
    let toolbox = runtime
        .toolbox(store, std::sync::Arc::new(graph_core::NullSink))
        .await?;
    let result = dispatch(toolbox.as_ref(), command).await;
    runtime.shutdown().await;
    result
}

async fn dispatch(
    registry: &(dyn ToolRegistry + Send + Sync),
    command: ToolsCommand,
) -> Result<()> {
    match command {
        ToolsCommand::List => {
            let defs = registry.tools().await?;
            if defs.is_empty() {
                println!("no tools available — configure [mcp.*] servers");
                return Ok(());
            }
            for def in defs {
                println!("{}", def.name);
            }
            Ok(())
        }
        ToolsCommand::Show { name } => {
            let defs = registry.tools().await?;
            let Some(def) = defs.into_iter().find(|d| d.name == name) else {
                bail!("unknown tool: {name}");
            };
            println!("{}\n\n{}\n", def.name, def.description);
            println!(
                "input schema:\n{}",
                serde_json::to_string_pretty(&def.input_schema)?
            );
            if let Some(schema) = def.output_schema {
                println!(
                    "\noutput schema:\n{}",
                    serde_json::to_string_pretty(&schema)?
                );
            }
            Ok(())
        }
        ToolsCommand::Test { name, inputs } => {
            let input = parse_inputs(&inputs)?;
            let outcome = registry.invoke(&name, input).await?;
            if outcome.is_error {
                eprintln!("tool returned an error:");
            }
            println!("{}", serde_json::to_string_pretty(&outcome.result)?);
            Ok(())
        }
    }
}

/// key=value pairs → JSON object; values parse as JSON when possible,
/// otherwise as strings.
pub fn parse_inputs(pairs: &[String]) -> Result<serde_json::Value> {
    let mut map = serde_json::Map::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            bail!("--input must be key=value, got: {pair}");
        };
        let parsed = serde_json::from_str(value)
            .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
        map.insert(key.to_string(), parsed);
    }
    Ok(serde_json::Value::Object(map))
}
