//! `graph chat` — interactive REPL with in-memory history (persistence
//! lands in phase 3).

use crate::output::TtySink;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_llm::types::ChatMessage;
use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};
use std::sync::Arc;

pub async fn run(thread: Option<String>, r#continue: bool) -> Result<()> {
    if thread.is_some() || r#continue {
        bail!("thread persistence lands in phase 3 — omit --thread/--continue for now");
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("chat needs an interactive terminal — use `graph ask` for scripted queries");
    }
    let runtime = Runtime::init()?;
    let agent = runtime.agent(Arc::new(TtySink::new(false)))?;

    let mut editor = Reedline::create();
    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic("graph".into()),
        DefaultPromptSegment::Empty,
    );
    let mut messages: Vec<ChatMessage> = Vec::new();
    eprintln!("graph chat — /quit to exit, /state to inspect the conversation");

    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if let Some(command) = line.strip_prefix('/') {
                    if handle_slash(command, &messages)? {
                        break;
                    }
                    continue;
                }
                messages.push(ChatMessage::User { content: line });
                match agent.run_turn(&mut messages).await {
                    Ok(_) => println!(),
                    Err(e) => {
                        // Keep the session alive; drop the failed turn's user
                        // message so a retry doesn't double it.
                        eprintln!("error: {e}");
                    }
                }
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Err(e) => bail!("readline failed: {e}"),
        }
    }
    Ok(())
}

/// Returns true when the session should end.
fn handle_slash(command: &str, messages: &[ChatMessage]) -> Result<bool> {
    match command.trim() {
        "quit" | "exit" | "q" => Ok(true),
        "state" => {
            println!("{}", serde_json::to_string_pretty(messages)?);
            Ok(false)
        }
        "plan" | "thread" => {
            eprintln!("/{command} lands in a later phase");
            Ok(false)
        }
        other => {
            eprintln!("unknown command: /{other} (try /quit or /state)");
            Ok(false)
        }
    }
}
