//! `graph chat` — interactive REPL with persistent threads.

use crate::output::TtySink;
use crate::runtime::{resolve_thread, title_from, Runtime};
use anyhow::{bail, Result};
use graph_core::{Store, ThreadMeta};
use graph_llm::types::ChatMessage;
use graph_store::GraphStore;
use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};
use std::sync::Arc;

pub async fn run(thread: Option<Option<String>>) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("chat needs an interactive terminal — use `graph ask` for scripted queries");
    }
    let runtime = Runtime::init()?;
    let store = runtime.store()?;
    let mut thread: Option<ThreadMeta> = resolve_thread(store.as_ref(), thread).await?;
    let events: Arc<dyn graph_core::EventSink> = Arc::new(TtySink::new(false));
    let toolbox = runtime.toolbox(store.clone(), events.clone()).await?;
    let agent = runtime.agent(events, toolbox)?;

    let mut messages: Vec<ChatMessage> = match &thread {
        Some(meta) => {
            eprintln!("continuing thread {} — {}", meta.id, meta.title);
            store.load_messages(&meta.id).await?
        }
        None => Vec::new(),
    };

    let mut editor = Reedline::create();
    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic("graph".into()),
        DefaultPromptSegment::Empty,
    );
    eprintln!("graph chat — /quit to exit, /state to inspect, /thread for the thread id");

    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if let Some(command) = line.strip_prefix('/') {
                    if handle_slash(command, &messages, &thread)? {
                        break;
                    }
                    continue;
                }
                let pre_len = messages.len();
                messages.push(ChatMessage::User {
                    content: line.clone(),
                });
                match agent.run_turn(&mut messages).await {
                    Ok(_) => {
                        println!();
                        if let Err(e) =
                            persist_turn(store.as_ref(), &mut thread, &line, &messages[pre_len..])
                                .await
                        {
                            eprintln!("warning: failed to persist turn: {e}");
                        }
                    }
                    Err(e) => {
                        // Drop the failed turn's messages so a retry starts clean.
                        messages.truncate(pre_len);
                        eprintln!("error: {e}");
                    }
                }
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Err(e) => {
                runtime.shutdown().await;
                bail!("readline failed: {e}");
            }
        }
    }
    runtime.shutdown().await;
    if let Some(meta) = &thread {
        eprintln!("thread {} — resume with `graph chat --thread`", meta.id);
    }
    Ok(())
}

async fn persist_turn(
    store: &GraphStore,
    thread: &mut Option<ThreadMeta>,
    first_message: &str,
    new_messages: &[ChatMessage],
) -> Result<()> {
    if thread.is_none() {
        *thread = Some(store.create_thread(&title_from(first_message)).await?);
    }
    let meta = thread.as_ref().unwrap();
    store.append_messages(&meta.id, new_messages).await?;
    Ok(())
}

/// Returns true when the session should end.
fn handle_slash(
    command: &str,
    messages: &[ChatMessage],
    thread: &Option<ThreadMeta>,
) -> Result<bool> {
    match command.trim() {
        "quit" | "exit" | "q" => Ok(true),
        "state" => {
            println!("{}", serde_json::to_string_pretty(messages)?);
            Ok(false)
        }
        "thread" => {
            match thread {
                Some(meta) => println!("{} — {}", meta.id, meta.title),
                None => println!("no thread yet (created after the first turn)"),
            }
            Ok(false)
        }
        "plan" => {
            eprintln!("/plan lands in a later phase");
            Ok(false)
        }
        other => {
            eprintln!("unknown command: /{other} (try /quit, /state, /thread)");
            Ok(false)
        }
    }
}
