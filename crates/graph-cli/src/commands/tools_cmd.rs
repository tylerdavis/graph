//! `graph tools` — inspect the tool catalog.

use crate::cli::ToolsCommand;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::ToolRegistry;

pub async fn run(command: ToolsCommand) -> Result<()> {
    let runtime = Runtime::init()?;
    let handles = runtime.store_handles()?;
    let toolbox = runtime
        .toolbox(&handles, std::sync::Arc::new(graph_core::NullSink))
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
            print!(
                "{}",
                super::listing::render_tool_listing(
                    &defs,
                    std::io::IsTerminal::is_terminal(&std::io::stdout())
                )
            );
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
        ToolsCommand::Test {
            name,
            input,
            inputs,
        } => {
            let input = crate::commands::input::resolve_input(input.as_deref(), &inputs)?;
            let outcome = registry.invoke(&name, input).await?;
            if outcome.is_error {
                eprintln!("tool returned an error:");
            }
            println!("{}", serde_json::to_string_pretty(&outcome.result)?);
            Ok(())
        }
    }
}
