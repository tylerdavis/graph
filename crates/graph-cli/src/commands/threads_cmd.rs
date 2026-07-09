//! `graph threads` — list/show/rm. Opens the store directly; no providers
//! or MCP servers needed.

use crate::cli::ThreadsCommand;
use crate::runtime::open_store;
use anyhow::{bail, Result};
use graph_llm::types::ChatMessage;

pub async fn run(command: ThreadsCommand) -> Result<()> {
    let config = graph_config::load()?.config;
    let store = open_store(&config)?;

    match command {
        ThreadsCommand::List => {
            let threads = store.list_threads().await?;
            if threads.is_empty() {
                println!("no threads yet — run `graph ask` or `graph chat`");
                return Ok(());
            }
            for thread in threads {
                println!(
                    "{}  {}  {:>3} msgs  {}",
                    thread.id,
                    format_time(thread.updated_at),
                    thread.message_count,
                    thread.title,
                );
            }
            Ok(())
        }
        ThreadsCommand::Show { id, state } => {
            let Some(meta) = store.get_thread(&id).await? else {
                bail!("no thread with id {id}");
            };
            let messages = store.load_messages(&id).await?;
            if state {
                println!("{}", serde_json::to_string_pretty(&messages)?);
                return Ok(());
            }
            println!(
                "{} — {} ({} messages, updated {})\n",
                meta.id,
                meta.title,
                meta.message_count,
                format_time(meta.updated_at),
            );
            for message in &messages {
                println!("{}", render_message(message));
            }
            Ok(())
        }
        ThreadsCommand::Rm { id } => {
            if store.delete_thread(&id).await? {
                println!("deleted {id}");
                Ok(())
            } else {
                bail!("no thread with id {id}");
            }
        }
    }
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
