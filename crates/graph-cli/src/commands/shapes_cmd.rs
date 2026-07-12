//! `graph shapes` — inspect the observed-shape cache. Opens the store
//! directly; no providers or MCP servers needed.

use crate::cli::ShapesCommand;
use crate::runtime::open_store;
use anyhow::{bail, Result};

pub async fn run(command: ShapesCommand) -> Result<()> {
    let config = graph_config::load()?.config;
    let store = open_store(&config)?;
    let shapes = store.tool_shapes().await?;

    match command {
        ShapesCommand::List { json } => {
            if json {
                let out: Vec<serde_json::Value> = shapes
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "tool": s.tool,
                            "seen_count": s.seen_count,
                            "schema": s.schema,
                            "example": s.example,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            if shapes.is_empty() {
                println!("no shapes cached yet — they record as tools run");
                return Ok(());
            }
            for shape in shapes {
                println!("{}  seen {}×", shape.tool, shape.seen_count);
            }
            Ok(())
        }
        ShapesCommand::Show { tool } => {
            let Some(shape) = shapes.into_iter().find(|s| s.tool == tool) else {
                bail!("no cached shape for tool {tool}");
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "tool": shape.tool,
                    "seen_count": shape.seen_count,
                    "schema": shape.schema,
                    "example": shape.example,
                }))?
            );
            Ok(())
        }
    }
}
