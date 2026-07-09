//! `graph ask` — one agent turn.

use crate::output::TtySink;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::NullSink;
use graph_llm::types::ChatMessage;
use std::io::{IsTerminal, Read};
use std::sync::Arc;

pub struct AskArgs {
    pub message: Option<String>,
    pub thread: Option<String>,
    pub r#continue: bool,
    pub json: bool,
    pub no_stream: bool,
}

pub async fn run(args: AskArgs) -> Result<()> {
    if args.thread.is_some() || args.r#continue {
        bail!("thread persistence lands in phase 3 — omit --thread/--continue for now");
    }
    let message = resolve_message(args.message)?;

    let runtime = Runtime::init()?;
    let stream_text = !args.json && !args.no_stream;
    let events: Arc<dyn graph_core::EventSink> = if args.json {
        Arc::new(NullSink)
    } else {
        Arc::new(TtySink::new(!stream_text))
    };
    let agent = runtime.agent(events)?;

    let mut messages = vec![ChatMessage::User { content: message }];
    let outcome = agent.run_turn(&mut messages).await?;

    if args.json {
        let envelope = serde_json::json!({
            "content": outcome.text,
            "tool_calls_made": outcome.tool_calls_made,
            "usage": outcome.usage,
            "thread_id": null,
        });
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else if stream_text {
        println!();
    } else {
        println!("{}", outcome.text);
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
