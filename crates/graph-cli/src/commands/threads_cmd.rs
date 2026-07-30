//! `graph threads` — list/show/rm. Opens the store directly; no providers
//! or MCP servers needed.

use crate::cli::ThreadsCommand;
use crate::commands::outcome::{report, Outcome};
use crate::runtime::open_store;
use anyhow::{bail, Result};
use graph_core::Store;
use graph_llm::types::ChatMessage;
use serde_json::json;
use std::sync::Arc;

pub async fn run(command: ThreadsCommand) -> Result<()> {
    let config = graph_config::load()?.config;
    let store = open_store(&config)?;

    match command {
        ThreadsCommand::List { json } => report(list(&store).await?, json),
        ThreadsCommand::Show { id, state, json } => report(show(&store, &id, state).await?, json),
        ThreadsCommand::Rm { id, json } => report(rm(&store, &id).await?, json),
    }
}

async fn list(store: &Arc<dyn Store>) -> Result<Outcome> {
    let threads = store.list_threads().await?;
    let body = json!({
        "threads": threads.iter().map(|thread| json!({
            "id": thread.id,
            "title": thread.title,
            "messageCount": thread.message_count,
            "updatedAt": thread.updated_at,
        })).collect::<Vec<_>>(),
        "count": threads.len(),
    });
    if threads.is_empty() {
        return Ok(Outcome::raw(String::new(), body)
            .with_note("no threads yet — run `graph ask` or `graph chat`"));
    }
    let text = threads
        .iter()
        .map(|thread| {
            format!(
                "{}  {}  {:>3} msgs  {}\n",
                thread.id,
                format_time(thread.updated_at),
                thread.message_count,
                thread.title,
            )
        })
        .collect::<String>();
    Ok(Outcome::raw(text, body))
}

async fn show(store: &Arc<dyn Store>, id: &str, state: bool) -> Result<Outcome> {
    let Some(meta) = store.get_thread(id).await? else {
        bail!("no thread with id {id}");
    };
    let messages = store.load_messages(id).await?;
    let body = json!({
        "id": meta.id,
        "title": meta.title,
        "messageCount": meta.message_count,
        "updatedAt": meta.updated_at,
        "messages": messages,
    });
    // `--state` asks for the raw runtime state, so it prints the messages
    // verbatim rather than the conversation transcript.
    if state {
        let text = format!("{}\n", serde_json::to_string_pretty(&messages)?);
        return Ok(Outcome::raw(text, body));
    }
    let mut text = format!(
        "{} — {} ({} messages, updated {})\n\n",
        meta.id,
        meta.title,
        meta.message_count,
        format_time(meta.updated_at),
    );
    for message in &messages {
        text.push_str(&render_message(message));
        text.push('\n');
    }
    Ok(Outcome::raw(text, body))
}

async fn rm(store: &Arc<dyn Store>, id: &str) -> Result<Outcome> {
    if !store.delete_thread(id).await? {
        bail!("no thread with id {id}");
    }
    Ok(Outcome::raw(
        format!("deleted {id}\n"),
        json!({"deleted": id}),
    ))
}

fn format_time(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string())
}

fn render_message(message: &ChatMessage) -> String {
    match message {
        ChatMessage::User { content } => format!("user> {content}\n"),
        ChatMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut out = String::new();
            for call in tool_calls {
                let args = serde_json::to_string(&call.arguments).unwrap_or_default();
                let preview: String = args.chars().take(100).collect();
                out.push_str(&format!("  → {} {}\n", call.name, preview));
            }
            if let Some(text) = content {
                if !text.is_empty() {
                    out.push_str(&format!("assistant> {text}\n"));
                }
            }
            out
        }
        ChatMessage::ToolResult {
            content, is_error, ..
        } => {
            let rendered = serde_json::to_string(content).unwrap_or_default();
            let preview: String = rendered.chars().take(160).collect();
            let marker = if *is_error { "✗" } else { "✓" };
            format!("  {marker} {preview}\n")
        }
    }
}
