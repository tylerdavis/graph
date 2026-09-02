//! `graph shapes` — inspect the observed-shape cache. Opens the store
//! directly; no providers or MCP servers needed.

use crate::cli::ShapesCommand;
use crate::commands::outcome::{report, Outcome};
use crate::runtime::open_store;
use anyhow::{bail, Result};
use graph_core::ToolShape;
use serde_json::{json, Value};

pub async fn run(command: ShapesCommand) -> Result<()> {
    let config = crate::runtime::load_config()?.config;
    let store = open_store(&config)?;
    let shapes = store.tool_shapes().await?;

    match command {
        ShapesCommand::List { json } => report(list(shapes), json),
        ShapesCommand::Show { tool, json } => report(show(shapes, &tool)?, json),
    }
}

fn shape_as_json(shape: &ToolShape) -> Value {
    json!({
        "tool": shape.tool,
        "seen_count": shape.seen_count,
        "schema": shape.schema,
        "example": shape.example,
    })
}

fn list(shapes: Vec<ToolShape>) -> Outcome {
    let body = json!({
        "shapes": shapes.iter().map(shape_as_json).collect::<Vec<_>>(),
        "count": shapes.len(),
    });
    if shapes.is_empty() {
        return Outcome::raw(String::new(), body)
            .with_note("no shapes cached yet — they record as tools run");
    }
    let text = shapes
        .iter()
        .map(|shape| format!("{}  seen {}×\n", shape.tool, shape.seen_count))
        .collect::<String>();
    Outcome::raw(text, body)
}

fn show(shapes: Vec<ToolShape>, tool: &str) -> Result<Outcome> {
    let Some(shape) = shapes.into_iter().find(|s| s.tool == tool) else {
        // A bad argument, not a domain rejection: no envelope.
        bail!("no cached shape for tool {tool}");
    };
    let body = shape_as_json(&shape);
    // `shapes show` has always printed JSON with or without `--json` — the
    // cached schema and example *are* the deliverable, and there is no
    // sensible one-line rendering of them.
    let text = format!("{}\n", serde_json::to_string_pretty(&body)?);
    Ok(Outcome::raw(text, body))
}
