//! `graph ask` — one agent turn, persisted to a thread.

use crate::output::TtySink;
use crate::runtime::{resolve_thread, title_from, Runtime};
use anyhow::{bail, Result};
use graph_core::{NullSink, Store};
use graph_llm::types::ChatMessage;
use std::io::{IsTerminal, Read};
use std::sync::Arc;

pub struct AskArgs {
    pub message: Option<String>,
    pub thread: Option<Option<String>>,
    pub json: bool,
    pub no_stream: bool,
}

pub async fn run(args: AskArgs) -> Result<()> {
    let message = resolve_message(args.message)?;

    let runtime = Runtime::init()?;
    let store = runtime.store()?;
    let existing = resolve_thread(store.as_ref(), args.thread).await?;

    let stream_text = !args.json && !args.no_stream;
    let events: Arc<dyn graph_core::EventSink> = if args.json {
        Arc::new(NullSink)
    } else {
        Arc::new(TtySink::new(!stream_text))
    };
    let toolbox = runtime.toolbox(store.clone(), events.clone()).await?;
    let agent = runtime.agent(events, toolbox)?;

    let mut messages = match &existing {
        Some(thread) => store.load_messages(&thread.id).await?,
        None => Vec::new(),
    };
    let pre_len = messages.len();
    messages.push(ChatMessage::User {
        content: message.clone(),
    });

    let result = agent.run_turn(&mut messages).await;
    runtime.shutdown().await;
    let outcome = result?;

    // Persist only successful turns; a new thread is created on demand.
    let thread = match existing {
        Some(thread) => thread,
        None => store.create_thread(&title_from(&message)).await?,
    };
    store
        .append_messages(&thread.id, &messages[pre_len..])
        .await?;

    if args.json {
        let envelope = serde_json::json!({
            "content": outcome.text,
            "tool_calls_made": outcome.tool_calls_made,
            "usage": outcome.usage,
            "thread_id": thread.id,
        });
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        if stream_text {
            println!();
        } else {
            println!("{}", outcome.text);
        }
        eprintln!(
            "\x1b[2mthread {} — continue with `graph ask \"…\" --thread`\x1b[0m",
            thread.id
        );
    }
    Ok(())
}

/// Use the argument, appending piped stdin as an input block when present.
fn resolve_message(message: Option<String>) -> Result<String> {
    let mut piped = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_string(&mut piped)?;
    }
    let piped = piped.trim();
    match (message, piped.is_empty()) {
        (Some(m), true) => Ok(m),
        (Some(m), false) => Ok(format!("{m}\n\n<input>\n{piped}\n</input>")),
        (None, false) => Ok(piped.to_string()),
        (None, true) => bail!("no message given — pass one as an argument or pipe via stdin"),
    }
}
