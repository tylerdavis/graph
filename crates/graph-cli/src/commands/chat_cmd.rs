//! `graph chat` — interactive REPL with persistent threads.

use crate::runtime::{resolve_thread, starting_agent, title_from, Runtime};
use anyhow::{bail, Result};
use graph_core::agent::conversation::{view, Conversation};
use graph_core::{NewEntry, Store, ThreadMeta};
use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};
use std::sync::Arc;

pub async fn run(agent: Option<String>, thread: Option<Option<String>>) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("chat needs an interactive terminal — use `graph ask` for scripted queries");
    }
    let runtime = Runtime::init()?;
    let store = runtime.store()?;
    let mut thread: Option<ThreadMeta> = resolve_thread(store.as_ref(), thread).await?;
    let mut active = starting_agent(thread.as_ref(), agent.as_deref())?;
    let owner = thread
        .as_ref()
        .map(|meta| meta.owner.clone())
        .unwrap_or_else(|| active.clone());
    let run = crate::telemetry::RunInfo::conversation(
        "chat",
        thread.as_ref().map(|meta| meta.id.clone()),
    )
    .user(runtime.config.user.name.as_deref());
    let events: Arc<dyn graph_core::EventSink> = crate::output::make_sink(false, false, run);
    // A conversation already has the user's attention: a plan called as
    // plan__* from here can put an `ask` step's question to them.
    let hooks = crate::runtime::PipelineHooks {
        interlocutor: crate::interlocutor::tty(),
        ..Default::default()
    };
    runtime.usage.attach_events(events.clone());
    let conversation = runtime.conversation(&store, events.clone(), hooks).await?;
    conversation.agent(&active)?;

    let mut history: Vec<NewEntry> = match &thread {
        Some(meta) => {
            eprintln!(
                "continuing thread {} — {} (with {active})",
                meta.id, meta.title
            );
            store
                .load_entries(&meta.id)
                .await?
                .into_iter()
                .map(NewEntry::from)
                .collect()
        }
        None => Vec::new(),
    };

    let mut editor = Reedline::create();
    eprintln!(
        "graph chat with {active} — /quit to exit, /agent to see or switch agents, /state to inspect, /thread for the thread id"
    );

    loop {
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(prompt_label(&active)),
            DefaultPromptSegment::Empty,
        );
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if let Some(command) = line.strip_prefix('/') {
                    let session = Session {
                        conversation: &conversation,
                        store: store.as_ref(),
                        thread: &thread,
                        history: &history,
                    };
                    if session.slash(command, &mut active).await? {
                        break;
                    }
                    continue;
                }
                let created = thread.is_none();
                if created {
                    match store.create_thread(&title_from(&line), &owner).await {
                        Ok(meta) => {
                            if meta.active != active {
                                let _ = store.set_active_agent(&meta.id, &active).await;
                            }
                            thread = Some(meta);
                        }
                        Err(e) => eprintln!("warning: failed to create thread: {e}"),
                    }
                }
                events.run_started(&graph_core::RunStart {
                    name: "chat".to_string(),
                    session_id: thread.as_ref().map(|meta| meta.id.clone()),
                    input: Some(serde_json::Value::String(line.clone())),
                });
                let result = conversation.run_turn(&history, &active, &line).await;
                // Per turn, not per session: the ledger drains, so
                // each turn reports its own spend.
                let usage = runtime.usage.take();
                match result {
                    Ok(turn) => {
                        events.run_finished(&serde_json::Value::String(turn.text.clone()), false);
                        println!();
                        if !usage.is_empty() && !crate::output::jsonl_events() {
                            eprintln!("\x1b[2m{}\x1b[0m", usage.summary());
                        }
                        if turn.active != active {
                            eprintln!("\x1b[2mnow talking with {}\x1b[0m", turn.active);
                        }
                        if let Some(meta) = &thread {
                            if let Err(e) = persist_turn(store.as_ref(), meta, &active, &turn).await
                            {
                                eprintln!("warning: failed to persist turn: {e}");
                            }
                        }
                        active = turn.active;
                        history.extend(turn.entries);
                    }
                    Err(e) => {
                        events.run_finished(&serde_json::json!({"error": e.to_string()}), true);
                        if created {
                            if let Some(meta) = thread.take() {
                                let _ = store.delete_thread(&meta.id).await;
                            }
                        }
                        eprintln!("error: {e}");
                    }
                }
                if !usage.is_empty() {
                    events.usage_summary(&usage);
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
        eprintln!("resume with `graph chat --thread {}`", meta.id);
    }
    Ok(())
}

fn prompt_label(active: &str) -> String {
    if active == graph_core::agent::doc::CHAT_AGENT {
        "graph".to_string()
    } else {
        format!("graph:{active}")
    }
}

pub async fn persist_turn(
    store: &dyn Store,
    meta: &ThreadMeta,
    active: &str,
    turn: &graph_core::agent::conversation::ConversationTurn,
) -> Result<()> {
    store.append_entries(&meta.id, &turn.entries).await?;
    if turn.active != active {
        store.set_active_agent(&meta.id, &turn.active).await?;
    }
    Ok(())
}

struct Session<'a> {
    conversation: &'a Conversation,
    store: &'a dyn Store,
    thread: &'a Option<ThreadMeta>,
    history: &'a [NewEntry],
}

impl Session<'_> {
    /// Returns true when the session should end.
    async fn slash(&self, command: &str, active: &mut String) -> Result<bool> {
        let mut words = command.split_whitespace();
        match (words.next().unwrap_or_default(), words.next()) {
            ("quit" | "exit" | "q", _) => Ok(true),
            ("state", _) => {
                let messages = view(active, self.history.iter());
                println!("{}", serde_json::to_string_pretty(&messages)?);
                Ok(false)
            }
            ("thread", _) => {
                match self.thread {
                    Some(meta) => println!(
                        "{} — {} (owner {}, active {active})",
                        meta.id, meta.title, meta.owner
                    ),
                    None => println!("no thread yet (created after the first turn)"),
                }
                Ok(false)
            }
            ("agent", None) => {
                println!("talking with {active}");
                for doc in self.conversation.agents.iter() {
                    let marker = if doc.name == *active { "*" } else { " " };
                    println!("{marker} {} — {}", doc.name, doc.description);
                }
                Ok(false)
            }
            ("agent", Some(name)) => {
                match self.conversation.agent(name) {
                    Ok(doc) => {
                        *active = doc.name.clone();
                        if let Some(meta) = self.thread {
                            self.store.set_active_agent(&meta.id, active).await?;
                        }
                        println!("now talking with {active}");
                    }
                    Err(e) => eprintln!("{e}"),
                }
                Ok(false)
            }
            (other, _) => {
                eprintln!(
                    "unknown command: /{other} (try /quit, /agent, /agent <name>, /state, /thread)"
                );
                Ok(false)
            }
        }
    }
}
