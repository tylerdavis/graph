use super::doc::{global_fragments, input_problems, render_system_prompt, AgentDoc, AgentSet};
use super::task::{run_task_agent, TaskError, TaskSpec, TaskTools};
use super::{Agent, AgentError, EventSink};
use crate::pipeline::{named_agent_tool_def, Pipeline, AGENT_TOOL_PREFIX, MAX_SUBAGENT_DEPTH};
use crate::store::{EntryBody, NewEntry, USER_AUTHOR};
use crate::tools::{AllowlistRegistry, ToolDef, ToolError, ToolOutcome, ToolRegistry, ToolServer};
use crate::usage::CallSite;
use async_trait::async_trait;
use graph_llm::types::{ChatMessage, ToolCall, ToolSpec};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub const HANDOFF_PREFIX: &str = "transfer_to_";

struct Handoff {
    target: String,
    message: String,
    wait_for_user: bool,
    via: String,
}

pub const TRANSFER_BACK: &str = "transfer_back";

pub const CONTEXT_AUTHOR_PREFIX: &str = "context:";

pub const MAX_HANDOFFS_PER_TURN: usize = 4;

pub type ContextHook = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

pub type HandoffGuard = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct Conversation {
    pub agents: Arc<AgentSet>,
    pub catalog: Arc<dyn ToolRegistry>,
    pub pipeline: Arc<Pipeline>,
    pub events: Arc<dyn EventSink>,
    pub session: BTreeMap<&'static str, String>,
    pub prompt_overrides: BTreeMap<String, String>,
    pub context: Option<ContextHook>,
    pub default_max_iterations: u32,
    pub progress_tools: Vec<String>,
    pub handoff_guard: Option<HandoffGuard>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("no agent named '{name}' (available: {available})")]
    UnknownAgent { name: String, available: String },
    #[error("agent '{agent}': {message}")]
    Setup { agent: String, message: String },
    #[error("agent '{agent}': {source}")]
    Turn {
        agent: String,
        #[source]
        source: AgentError,
        partial: Vec<NewEntry>,
    },
}

#[derive(Debug)]
pub struct ConversationTurn {
    pub text: String,
    pub active: String,
    pub entries: Vec<NewEntry>,
    pub tool_calls_made: u32,
    pub tools_used: Vec<String>,
}

impl Conversation {
    pub fn agent(&self, name: &str) -> Result<AgentDoc, ConversationError> {
        self.agents
            .get(name)
            .ok_or_else(|| ConversationError::UnknownAgent {
                name: name.to_string(),
                available: self
                    .agents
                    .iter()
                    .map(|doc| doc.name)
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }

    pub async fn run_turn(
        &self,
        history: &[NewEntry],
        active: &str,
        user: &str,
    ) -> Result<ConversationTurn, ConversationError> {
        let mut entries = vec![NewEntry::message(
            USER_AUTHOR,
            ChatMessage::User {
                content: user.to_string(),
            },
        )];
        let mut active = active.to_string();
        let mut tool_calls_made = 0;
        let mut tools_used: Vec<String> = Vec::new();
        let mut handoffs = 0;
        loop {
            let doc = &self.agent(&active)?;
            let may_hand_off = handoffs < MAX_HANDOFFS_PER_TURN;
            let back_to = if may_hand_off {
                self.handed_over_by(&active, history.iter().chain(entries.iter()))
            } else {
                None
            };
            let tools =
                Arc::new(ConversationTools::new(self, doc, 0, may_hand_off).back_to(back_to));
            if let Some(note) = self.context_note(&active, history.iter().chain(entries.iter())) {
                entries.push(note);
            }
            let agent = self.build_agent(doc, tools.clone())?;
            let mut messages = view(&active, history.iter().chain(entries.iter()));
            let before = messages.len();
            let result = agent.run_turn(&mut messages).await;
            entries.extend(
                messages[before..]
                    .iter()
                    .cloned()
                    .map(|message| NewEntry::message(&active, message)),
            );
            entries.extend(tools.take_runs());
            let outcome = result.map_err(|source| ConversationError::Turn {
                agent: active.clone(),
                source,
                partial: entries.clone(),
            })?;
            tool_calls_made += outcome.tool_calls_made;
            for tool in outcome.tools_used {
                if !tools_used.contains(&tool) {
                    tools_used.push(tool);
                }
            }
            let Some(Handoff {
                target,
                message,
                wait_for_user,
                via,
            }) = tools.take_handoff()
            else {
                return Ok(ConversationTurn {
                    text: outcome.text,
                    active,
                    entries,
                    tool_calls_made,
                    tools_used,
                });
            };
            handoffs += 1;
            self.events.handoff(&active, &target, via == TRANSFER_BACK);
            entries.push(NewEntry {
                author: active.clone(),
                body: EntryBody::Handoff {
                    from: active.clone(),
                    to: target.clone(),
                    message: message.clone(),
                    via: Some(via),
                },
            });
            if wait_for_user {
                let text = if outcome.text.trim().is_empty() {
                    message
                } else {
                    outcome.text
                };
                return Ok(ConversationTurn {
                    text,
                    active: target,
                    entries,
                    tool_calls_made,
                    tools_used,
                });
            }
            active = target;
        }
    }

    fn context_note<'a>(
        &self,
        agent: &str,
        entries: impl Iterator<Item = &'a NewEntry>,
    ) -> Option<NewEntry> {
        let sections = self.context.as_ref().map(|context| context(agent))?;
        if sections.is_empty() {
            return None;
        }
        let author = format!("{CONTEXT_AUTHOR_PREFIX}{agent}");
        let content = sections.join("\n\n");
        let last = entries
            .filter(|entry| entry.author == author)
            .filter_map(|entry| match &entry.body {
                EntryBody::Message {
                    message: ChatMessage::User { content },
                } => Some(content.clone()),
                _ => None,
            })
            .last();
        (last.as_deref() != Some(content.as_str()))
            .then(|| NewEntry::message(&author, ChatMessage::User { content }))
    }

    fn system_prompt_with_context(&self, doc: &AgentDoc) -> Result<String, ConversationError> {
        let mut prompt = self.system_prompt(doc)?;
        if let Some(context) = &self.context {
            for section in context(&doc.name) {
                prompt.push_str("\n\n");
                prompt.push_str(&section);
            }
        }
        Ok(prompt)
    }

    fn system_prompt(&self, doc: &AgentDoc) -> Result<String, ConversationError> {
        let prompt = match self.prompt_overrides.get(&doc.name) {
            Some(prompt) => prompt.clone(),
            None => render_system_prompt(doc, &global_fragments(), &self.session).map_err(
                |message| ConversationError::Setup {
                    agent: doc.name.clone(),
                    message,
                },
            )?,
        };
        Ok(prompt)
    }

    fn build_agent(
        &self,
        doc: &AgentDoc,
        tools: Arc<ConversationTools>,
    ) -> Result<Agent, ConversationError> {
        let (provider, choice) = self
            .pipeline
            .router
            .resolve_named(&doc.model)
            .map_err(|e| ConversationError::Setup {
                agent: doc.name.clone(),
                message: e.to_string(),
            })?;
        Ok(Agent {
            provider,
            model: choice.model.clone(),
            temperature: choice.temperature,
            stop_tools: tools.handoff_tools(),
            registry: tools,
            events: self.events.clone(),
            system_prompt: self.system_prompt(doc)?,
            max_iterations: doc.max_iterations.unwrap_or(self.default_max_iterations),
            progress_tools: self.progress_tools.clone(),
            call_site: CallSite::role(doc.model.clone()).at(format!("agent:{}", doc.name)),
        })
    }

    async fn run_subagent(
        &self,
        caller: &str,
        name: &str,
        input: Value,
        depth: usize,
    ) -> (ToolOutcome, Option<NewEntry>) {
        let refuse = |message: String| {
            (
                ToolOutcome {
                    result: json!({ "error": message }),
                    is_error: true,
                },
                None,
            )
        };
        if depth > MAX_SUBAGENT_DEPTH {
            return refuse(format!(
                "agent '{name}' not started: subagents nest at most {MAX_SUBAGENT_DEPTH} deep"
            ));
        }
        let doc = match self.agent(name) {
            Ok(doc) => doc,
            Err(error) => return refuse(error.to_string()),
        };
        let doc = &doc;
        let problems = input_problems(&doc.subagent_input_schema(), &input);
        if !problems.is_empty() {
            return refuse(format!(
                "invalid input for agent '{name}': {}",
                problems.join("; ")
            ));
        }
        let system = match self.system_prompt_with_context(doc) {
            Ok(system) => system,
            Err(error) => return refuse(error.to_string()),
        };
        let prompt = match &doc.input_schema {
            None => input["prompt"].as_str().unwrap_or_default().to_string(),
            Some(_) => serde_json::to_string_pretty(&input).unwrap_or_default(),
        };
        let tools = ConversationTools::new(self, doc, depth, false);
        let specs: Vec<ToolSpec> = tools
            .tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|def| ToolSpec {
                name: def.name,
                description: def.description,
                input_schema: def.input_schema,
            })
            .collect();
        let spec = TaskSpec {
            model: doc.model.clone(),
            system,
            prompt,
            tools: specs,
            max_iterations: doc.max_iterations.or(Some(self.default_max_iterations)),
            output_schema: doc.output_schema.clone(),
        };
        let site = CallSite::role(doc.model.clone()).at(format!("agent:{caller}/agent__{name}"));
        let runner = RegistryTools {
            registry: &tools,
            events: self.events.as_ref(),
        };
        let run = run_task_agent(
            &spec,
            &self.pipeline.router,
            &site,
            self.events.as_ref(),
            &runner,
        )
        .await;
        let outcome = match run {
            Ok(outcome) => outcome,
            Err(TaskError::Failed(message)) => {
                return refuse(format!("agent '{name}' failed: {message}"))
            }
            Err(TaskError::Aborted(_)) => return refuse(format!("agent '{name}' was aborted")),
        };
        let result = if outcome.final_ {
            ToolOutcome {
                result: outcome.output.clone(),
                is_error: false,
            }
        } else {
            ToolOutcome {
                result: json!({
                    "error": format!("agent '{name}' ran out of rounds before answering"),
                    "partial": outcome.output,
                }),
                is_error: true,
            }
        };
        let entry = NewEntry {
            author: caller.to_string(),
            body: EntryBody::SubagentRun {
                agent: name.to_string(),
                caller: caller.to_string(),
                input,
                messages: outcome.messages,
                output: outcome.output,
                final_: outcome.final_,
            },
        };
        (result, Some(entry))
    }
}

pub fn view<'a>(agent: &str, entries: impl Iterator<Item = &'a NewEntry>) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut note: Vec<String> = Vec::new();
    let flush = |messages: &mut Vec<ChatMessage>, note: &mut Vec<String>| {
        if !note.is_empty() {
            messages.push(ChatMessage::User {
                content: note.join("\n\n"),
            });
            note.clear();
        }
    };
    for entry in entries {
        match &entry.body {
            EntryBody::Message { message } if entry.author == agent => {
                flush(&mut messages, &mut note);
                messages.push(message.clone());
            }
            EntryBody::Message {
                message: ChatMessage::User { content },
            } if entry.author == USER_AUTHOR => note.push(content.clone()),
            EntryBody::Message {
                message: ChatMessage::User { content },
            } if entry
                .author
                .strip_prefix(CONTEXT_AUTHOR_PREFIX)
                .is_some_and(|owner| owner == agent) =>
            {
                note.push(content.clone())
            }
            EntryBody::Message {
                message:
                    ChatMessage::Assistant {
                        content: Some(text),
                        ..
                    },
            } if !text.trim().is_empty() => note.push(format!("[{}] {text}", entry.author)),
            EntryBody::Handoff {
                from, to, message, ..
            } => note.push(format!(
                "[{from} handed the conversation to {to}] {message}"
            )),
            _ => {}
        }
    }
    flush(&mut messages, &mut note);
    messages
}

struct RegistryTools<'a> {
    registry: &'a dyn ToolRegistry,
    events: &'a dyn EventSink,
}

#[async_trait]
impl TaskTools for RegistryTools<'_> {
    async fn execute(
        &self,
        calls: &[ToolCall],
        _round: u32,
    ) -> Result<Vec<ChatMessage>, TaskError> {
        let runs = calls.iter().map(|call| async {
            self.events.tool_started(&call.name, &call.arguments);
            let started = std::time::Instant::now();
            let outcome = self
                .registry
                .invoke(&call.name, call.arguments.clone())
                .await
                .unwrap_or_else(|e| ToolOutcome {
                    result: json!({ "error": e.to_string() }),
                    is_error: true,
                });
            self.events
                .tool_finished(&call.name, started.elapsed(), outcome.is_error);
            ChatMessage::ToolResult {
                tool_call_id: call.id.clone(),
                content: outcome.result,
                is_error: outcome.is_error,
            }
        });
        Ok(futures::future::join_all(runs).await)
    }
}

struct ConversationTools {
    conversation: Conversation,
    base: AllowlistRegistry,
    subagents: Vec<ToolDef>,
    handoffs: Vec<String>,
    back_to: Option<String>,
    caller: String,
    depth: usize,
    runs: Mutex<Vec<NewEntry>>,
    handoff: Mutex<Option<Handoff>>,
}

impl Conversation {
    fn handed_over_by<'a>(
        &self,
        active: &str,
        entries: impl Iterator<Item = &'a NewEntry>,
    ) -> Option<String> {
        let last = entries
            .filter_map(|entry| match &entry.body {
                EntryBody::Handoff { from, to, via, .. } if to == active => {
                    Some((from.clone(), via.clone()))
                }
                _ => None,
            })
            .last()?;
        let (from, via) = last;
        let returned = via.as_deref() == Some(TRANSFER_BACK);
        (!returned && from != active && self.agents.get(&from).is_some()).then_some(from)
    }
}

impl ConversationTools {
    fn back_to(mut self, caller: Option<String>) -> Self {
        self.back_to = caller;
        self
    }

    fn new(conversation: &Conversation, doc: &AgentDoc, depth: usize, may_hand_off: bool) -> Self {
        Self {
            conversation: conversation.clone(),
            base: AllowlistRegistry::new(conversation.catalog.clone(), doc.tools.clone()),
            subagents: doc
                .subagents
                .iter()
                .filter_map(|name| conversation.agents.get(name))
                .map(|doc| named_agent_tool_def(&doc))
                .collect(),
            handoffs: doc
                .handoffs
                .iter()
                .filter(|name| may_hand_off && conversation.agents.get(name).is_some())
                .cloned()
                .collect(),
            back_to: None,
            caller: doc.name.clone(),
            depth,
            runs: Mutex::new(Vec::new()),
            handoff: Mutex::new(None),
        }
    }

    fn handoff_tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = self
            .handoffs
            .iter()
            .map(|name| format!("{HANDOFF_PREFIX}{name}"))
            .collect();
        if self.back_to.is_some() {
            tools.push(TRANSFER_BACK.to_string());
        }
        tools
    }

    fn back_def(caller: &str) -> ToolDef {
        ToolDef {
            name: TRANSFER_BACK.to_string(),
            description: format!(
                "Hand the conversation back to {caller}, the agent that handed it to you, once \
                 you've done what it handed over. {caller} continues this turn with your \
                 message; say what you did and anything it needs to pick up from here. Your \
                 turn ends when you call it."
            ),
            input_schema: json!({
                "type": "object",
                "required": ["message"],
                "properties": {
                    "message": {
                        "type": "string",
                        "description": format!("What you did and what {caller} needs to continue")
                    },
                    "wait_for_user": {
                        "type": "boolean",
                        "description": format!("True to end the turn here and let {caller} answer the user's next message; false (the default) to have {caller} continue this turn now")
                    }
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        }
    }

    fn take_runs(&self) -> Vec<NewEntry> {
        std::mem::take(&mut self.runs.lock().unwrap())
    }

    fn take_handoff(&self) -> Option<Handoff> {
        self.handoff.lock().unwrap().take()
    }

    fn handoff_def(target: &str) -> ToolDef {
        ToolDef {
            name: format!("{HANDOFF_PREFIX}{target}"),
            description: format!(
                "Hand the conversation to the {target} agent, which continues this turn with the \
                 user and everything said so far. Use it when {target} is the right agent for \
                 what the user needs now; your turn ends when you call it. With wait_for_user, \
                 {target} does not run now: it takes over from the user's next message, and any \
                 text you send with this call is your reply to the user."
            ),
            input_schema: json!({
                "type": "object",
                "required": ["message"],
                "properties": {
                    "message": {
                        "type": "string",
                        "description": format!("What {target} needs to know to pick up from here")
                    },
                    "wait_for_user": {
                        "type": "boolean",
                        "description": format!("True to end the turn here and let {target} answer the user's next message; false (the default) to have {target} continue this turn now")
                    }
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        }
    }
}

#[async_trait]
impl ToolRegistry for ConversationTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut defs = self.base.tools().await?;
        defs.extend(self.subagents.iter().cloned());
        defs.extend(self.handoffs.iter().map(|name| Self::handoff_def(name)));
        if let Some(caller) = &self.back_to {
            defs.push(Self::back_def(caller));
        }
        Ok(defs)
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let target = match (name, &self.back_to) {
            (TRANSFER_BACK, Some(caller)) => Some(caller.clone()),
            _ => name
                .strip_prefix(HANDOFF_PREFIX)
                .filter(|target| self.handoffs.iter().any(|handoff| handoff == target))
                .map(str::to_string),
        };
        if let (Some(target), Some(guard)) = (&target, &self.conversation.handoff_guard) {
            if let Some(refusal) = guard(&self.caller, target) {
                return Ok(ToolOutcome {
                    result: json!({ "error": refusal }),
                    is_error: true,
                });
            }
        }
        if let (TRANSFER_BACK, Some(caller)) = (name, &self.back_to) {
            *self.handoff.lock().unwrap() = Some(Handoff {
                target: caller.clone(),
                message: input["message"].as_str().unwrap_or_default().to_string(),
                wait_for_user: input["wait_for_user"].as_bool().unwrap_or(false),
                via: TRANSFER_BACK.to_string(),
            });
            return Ok(ToolOutcome {
                result: json!({ "transferred_to": caller }),
                is_error: false,
            });
        }
        if let Some(target) = name.strip_prefix(HANDOFF_PREFIX) {
            if self.handoffs.iter().any(|handoff| handoff == target) {
                let message = input["message"].as_str().unwrap_or_default().to_string();
                let wait_for_user = input["wait_for_user"].as_bool().unwrap_or(false);
                *self.handoff.lock().unwrap() = Some(Handoff {
                    target: target.to_string(),
                    message,
                    wait_for_user,
                    via: name.to_string(),
                });
                return Ok(ToolOutcome {
                    result: json!({ "transferred_to": target }),
                    is_error: false,
                });
            }
        }
        if let Some(agent) = name.strip_prefix(AGENT_TOOL_PREFIX) {
            if self.subagents.iter().any(|def| def.name == name) {
                let (outcome, entry) = Box::pin(self.conversation.run_subagent(
                    &self.caller,
                    agent,
                    input,
                    self.depth + 1,
                ))
                .await;
                if let Some(entry) = entry {
                    self.runs.lock().unwrap().push(entry);
                }
                return Ok(outcome);
            }
        }
        self.base.invoke(name, input).await
    }

    async fn servers(&self) -> Vec<ToolServer> {
        self.base.servers().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::doc::builtin_agents;
    use futures::StreamExt;
    use graph_config::{ModelChoice, ModelRoles};
    use graph_llm::types::{
        ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, ToolCall, Usage,
    };
    use graph_llm::{ChatProvider, LlmError};
    use std::collections::HashMap;

    struct Scripted {
        responses: Mutex<Vec<ChatResponse>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl Scripted {
        fn next(&self, req: ChatRequest) -> ChatResponse {
            self.requests.lock().unwrap().push(req);
            self.responses.lock().unwrap().remove(0)
        }
    }

    #[async_trait]
    impl ChatProvider for Scripted {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(self.next(req))
        }
        async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
            let response = self.next(req);
            Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(response))]).boxed())
        }
    }

    struct Echo;

    #[async_trait]
    impl ToolRegistry for Echo {
        async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
            Ok(vec![ToolDef {
                name: "t__echo".into(),
                description: "echo".into(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            }])
        }
        async fn invoke(&self, _: &str, input: Value) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome {
                result: json!({"echoed": input}),
                is_error: false,
            })
        }
    }

    fn text(content: &str) -> ChatResponse {
        ChatResponse {
            content: Some(content.into()),
            tool_calls: vec![],
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn call(id: &str, name: &str, arguments: Value) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            }],
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    const AGENTS: &[&str] = &[
        "name: front\ndescription: routes\nmodel: chat\ntools: [t__echo]\nhandoffs: [back, drafter]\nsubagents: [helper]\nsystem_prompt: You are front.\n",
        "name: drafter\ndescription: drafts\nmodel: chat\ntools: []\nsystem_prompt: You draft.\n",
        "name: back\ndescription: works\nmodel: chat\ntools: []\nhandoffs: [front]\nsystem_prompt: You are back.\n",
        "name: helper\ndescription: helps\nmodel: chat\ntools: [t__echo]\nsystem_prompt: You help.\n",
    ];

    fn conversation(responses: Vec<ChatResponse>) -> (Conversation, Arc<Scripted>) {
        let provider = Arc::new(Scripted {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        });
        let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
        providers.insert("mock".into(), provider.clone());
        let router = Arc::new(graph_llm::ModelRouter::with_providers(
            providers,
            ModelRoles::from([(
                "default",
                ModelChoice {
                    provider: "mock".into(),
                    model: "m".into(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                },
            )]),
        ));
        let agents = Arc::new(AgentSet::layered(builtin_agents(AGENTS), Vec::new()));
        let pipeline = Arc::new(Pipeline {
            router,
            registry: Arc::new(Echo),
            events: Arc::new(crate::NullSink),
            plans: Arc::new(Vec::new()),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: String::new(),
            current_date: "2026-09-30".into(),
            max_attempts: 1,
            max_agent_iterations: 15,
            usage: Arc::new(crate::usage::UsageLedger::unpriced()),
            agents: agents.clone(),
            agent_depth: 0,
            always_loaded: Default::default(),
            drafted: Default::default(),
        });
        let conversation = Conversation {
            agents,
            catalog: Arc::new(Echo),
            pipeline,
            events: Arc::new(crate::NullSink),
            session: BTreeMap::from([("date", "today".to_string()), ("user", String::new())]),
            prompt_overrides: BTreeMap::new(),
            context: None,
            default_max_iterations: 8,
            progress_tools: Vec::new(),
            handoff_guard: None,
        };
        (conversation, provider)
    }

    fn kinds(entries: &[NewEntry]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|entry| {
                let kind = serde_json::to_value(&entry.body).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string();
                (entry.author.clone(), kind)
            })
            .collect()
    }

    fn user_text(request: &ChatRequest) -> String {
        request
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::User { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_waiting_handoff_ends_the_turn_with_the_target_active() {
        let mut confirm = call(
            "c1",
            "transfer_to_back",
            json!({"message": "the plan is drafted", "wait_for_user": true}),
        );
        confirm.content = Some("Drafted the plan.".into());
        let (conversation, provider) =
            conversation(vec![confirm, text("the target must not run yet")]);
        let turn = conversation
            .run_turn(&[], "front", "draft me a plan")
            .await
            .unwrap();

        assert_eq!(turn.active, "back");
        assert_eq!(turn.text, "Drafted the plan.");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "the target agent waits for the user's next message"
        );
        assert_eq!(kinds(&turn.entries).last().unwrap().1, "handoff");
    }

    #[tokio::test]
    async fn a_handoff_continues_the_turn_in_the_target_agent() {
        let (conversation, provider) = conversation(vec![
            call(
                "c1",
                "transfer_to_back",
                json!({"message": "they need the report"}),
            ),
            text("here is the report"),
        ]);
        let turn = conversation
            .run_turn(&[], "front", "report please")
            .await
            .unwrap();

        assert_eq!(turn.active, "back");
        assert_eq!(turn.text, "here is the report");
        let expected: Vec<(String, String)> = [
            ("user", "message"),
            ("front", "message"),
            ("front", "message"),
            ("front", "handoff"),
            ("back", "message"),
        ]
        .iter()
        .map(|(author, kind)| (author.to_string(), kind.to_string()))
        .collect();
        assert_eq!(kinds(&turn.entries), expected);

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].system, "You are front.");
        let front_tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            front_tools,
            [
                "t__echo",
                "agent__helper",
                "transfer_to_back",
                "transfer_to_drafter"
            ]
        );
        assert_eq!(requests[1].system, "You are back.");
        assert_eq!(requests[1].messages.len(), 1, "back sees one merged note");
        let note = user_text(&requests[1]);
        assert!(note.contains("report please"), "{note}");
        assert!(
            note.contains("[front handed the conversation to back] they need the report"),
            "{note}"
        );
    }

    fn tool_names(request: &ChatRequest) -> Vec<String> {
        request.tools.iter().map(|tool| tool.name.clone()).collect()
    }

    struct Recorder(Mutex<Vec<String>>);

    impl crate::EventSink for Recorder {
        fn text_delta(&self, text: &str) {
            self.0.lock().unwrap().push(format!("text {text}"));
        }
        fn handoff(&self, from: &str, to: &str, back: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("handoff {from}->{to} back={back}"));
        }
    }

    #[tokio::test]
    async fn a_handoff_is_announced_when_it_happens_not_when_the_turn_ends() {
        let (mut conversation, _) = conversation(vec![
            call("c1", "transfer_to_drafter", json!({"message": "draft it"})),
            call("c2", "transfer_back", json!({"message": "done"})),
            text("back with you"),
        ]);
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        conversation.events = recorder.clone();
        conversation.run_turn(&[], "front", "go").await.unwrap();
        let events = recorder.0.lock().unwrap().clone();
        let handoffs: Vec<&String> = events.iter().filter(|e| e.starts_with("handoff")).collect();
        assert_eq!(
            handoffs,
            [
                "handoff front->drafter back=false",
                "handoff drafter->front back=true"
            ]
        );
    }

    #[tokio::test]
    async fn an_agent_handed_the_conversation_can_hand_it_back() {
        let (conversation, provider) = conversation(vec![
            call(
                "c1",
                "transfer_to_drafter",
                json!({"message": "make a weather tool"}),
            ),
            call(
                "c2",
                "transfer_back",
                json!({"message": "saved user__weather"}),
            ),
            text("redrafting with user__weather"),
        ]);
        let turn = conversation
            .run_turn(&[], "front", "plan my day around the weather")
            .await
            .unwrap();

        assert_eq!(turn.active, "front");
        assert_eq!(turn.text, "redrafting with user__weather");
        let requests = provider.requests.lock().unwrap();
        assert!(tool_names(&requests[1]).contains(&"transfer_back".to_string()));
        assert!(
            !tool_names(&requests[2]).contains(&"transfer_back".to_string()),
            "handing back doesn't give the caller a way back to the drafter"
        );
        let note = user_text(&requests[2]);
        assert!(
            note.contains("[drafter handed the conversation to front] saved user__weather"),
            "{note}"
        );
        let back = turn
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.body {
                EntryBody::Handoff { via, .. } => via.clone(),
                _ => None,
            });
        assert_eq!(back.as_deref(), Some(TRANSFER_BACK));
    }

    #[tokio::test]
    async fn a_guard_can_refuse_a_handoff_and_the_agent_carries_on() {
        let (mut conversation, provider) = conversation(vec![
            call(
                "c1",
                "transfer_to_drafter",
                json!({"message": "make a tool"}),
            ),
            text("I'll save the draft first."),
        ]);
        conversation.handoff_guard = Some(Arc::new(|from, to| {
            (from == "front" && to == "drafter")
                .then(|| "save or discard the draft first".to_string())
        }));
        let turn = conversation.run_turn(&[], "front", "go").await.unwrap();
        assert_eq!(turn.active, "front");
        assert_eq!(turn.text, "I'll save the draft first.");
        let requests = provider.requests.lock().unwrap();
        assert!(format!("{:?}", requests[1].messages).contains("save or discard the draft first"));
    }

    #[tokio::test]
    async fn an_agent_started_directly_has_no_one_to_hand_back_to() {
        let (conversation, provider) = conversation(vec![text("drafting")]);
        conversation
            .run_turn(&[], "drafter", "make a tool")
            .await
            .unwrap();
        let requests = provider.requests.lock().unwrap();
        assert!(!tool_names(&requests[0]).contains(&"transfer_back".to_string()));
    }

    #[tokio::test]
    async fn the_way_back_survives_across_turns() {
        let (conversation, provider) = conversation(vec![
            call(
                "c1",
                "transfer_to_drafter",
                json!({"message": "make a weather tool", "wait_for_user": true}),
            ),
            text("which city?"),
        ]);
        let first = conversation
            .run_turn(&[], "front", "plan my day around the weather")
            .await
            .unwrap();
        assert_eq!(first.active, "drafter");
        conversation
            .run_turn(&first.entries, "drafter", "Denver")
            .await
            .unwrap();
        let requests = provider.requests.lock().unwrap();
        assert!(tool_names(&requests[1]).contains(&"transfer_back".to_string()));
    }

    #[tokio::test]
    async fn a_subagent_run_is_logged_but_never_enters_a_view() {
        let (conversation, provider) = conversation(vec![
            call("c1", "agent__helper", json!({"prompt": "look it up"})),
            text("found it"),
            text("the answer"),
        ]);
        let turn = conversation
            .run_turn(&[], "front", "question")
            .await
            .unwrap();
        assert_eq!(turn.text, "the answer");
        let (agent, caller, output, final_, count) = turn
            .entries
            .iter()
            .find_map(|entry| match &entry.body {
                EntryBody::SubagentRun {
                    agent,
                    caller,
                    output,
                    final_,
                    messages,
                    ..
                } => Some((
                    agent.clone(),
                    caller.clone(),
                    output.clone(),
                    *final_,
                    messages.len(),
                )),
                _ => None,
            })
            .expect("the subagent run is recorded");
        assert_eq!((agent.as_str(), caller.as_str()), ("helper", "front"));
        assert_eq!(output, json!({"result": "found it"}));
        assert!(final_);
        assert_eq!(count, 2);

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[1].system, "You help.");
        assert_eq!(user_text(&requests[1]), "look it up");

        assert_eq!(
            view("front", turn.entries.iter()).len(),
            4,
            "the caller sees its own call and result, never the subagent's messages"
        );
    }

    #[tokio::test]
    async fn context_rides_in_the_history_so_the_system_prompt_stays_cacheable() {
        let (mut conversation, provider) =
            conversation(vec![text("first"), text("second"), text("third")]);
        let draft = Arc::new(Mutex::new("v1".to_string()));
        let shared = draft.clone();
        conversation.context = Some(Arc::new(move |agent: &str| {
            if agent == "front" {
                vec![format!("## Current draft\n{}", shared.lock().unwrap())]
            } else {
                Vec::new()
            }
        }));
        let first = conversation.run_turn(&[], "front", "one").await.unwrap();
        let mut history = first.entries;
        let second = conversation
            .run_turn(&history, "front", "two")
            .await
            .unwrap();
        assert!(
            !second
                .entries
                .iter()
                .any(|entry| entry.author == "context:front"),
            "an unchanged context isn't repeated"
        );
        history.extend(second.entries);
        *draft.lock().unwrap() = "v2".to_string();
        conversation
            .run_turn(&history, "front", "three")
            .await
            .unwrap();

        let requests = provider.requests.lock().unwrap();
        assert!(requests
            .iter()
            .all(|request| request.system == requests[0].system));
        assert!(!requests[0].system.contains("Current draft"));
        assert!(user_text(&requests[0]).contains("one\n\n## Current draft\nv1"));
        let earlier: Vec<String> = requests[1]
            .messages
            .iter()
            .map(|m| format!("{m:?}"))
            .collect();
        let later: Vec<String> = requests[2]
            .messages
            .iter()
            .map(|m| format!("{m:?}"))
            .collect();
        assert_eq!(
            later[..earlier.len()],
            earlier[..],
            "earlier turns are a stable prefix"
        );
        assert!(user_text(&requests[2]).contains("## Current draft\nv2"));
    }

    #[tokio::test]
    async fn a_subagent_gets_the_conversation_catalog_and_its_context() {
        let (mut conversation, provider) = conversation(vec![
            call("c1", "agent__helper", json!({"prompt": "look it up"})),
            text("found it"),
            text("the answer"),
        ]);
        conversation.context = Some(Arc::new(|agent: &str| {
            if agent == "helper" {
                vec!["## Current draft\n(the draft)".to_string()]
            } else {
                Vec::new()
            }
        }));
        conversation
            .run_turn(&[], "front", "question")
            .await
            .unwrap();
        let requests = provider.requests.lock().unwrap();
        assert!(requests[1]
            .system
            .ends_with("## Current draft\n(the draft)"));
        assert!(!requests[0].system.contains("Current draft"));
        let tools: Vec<&str> = requests[1].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tools, ["t__echo"]);
    }

    #[test]
    fn a_view_keeps_other_agents_text_but_not_their_tool_traffic() {
        let entries = [
            NewEntry::message(
                USER_AUTHOR,
                ChatMessage::User {
                    content: "hi".into(),
                },
            ),
            NewEntry::message(
                "front",
                ChatMessage::Assistant {
                    content: Some("checking".into()),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "t__echo".into(),
                        arguments: json!({}),
                    }],
                    thinking: Vec::new(),
                },
            ),
            NewEntry::message(
                "front",
                ChatMessage::ToolResult {
                    tool_call_id: "c1".into(),
                    content: json!({"secret": 1}),
                    is_error: false,
                },
            ),
        ];
        let back = view("back", entries.iter());
        assert_eq!(back.len(), 1);
        let rendered = serde_json::to_string(&back).unwrap();
        assert!(rendered.contains("[front] checking"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");

        assert_eq!(
            view("front", entries.iter()).len(),
            3,
            "an agent sees its own tool calls and results"
        );
    }

    #[tokio::test]
    async fn after_four_handoffs_the_holder_must_answer() {
        let mut responses = Vec::new();
        for (i, target) in ["back", "front", "back", "front"].iter().enumerate() {
            responses.push(call(
                &format!("c{i}"),
                &format!("transfer_to_{target}"),
                json!({"message": "yours"}),
            ));
        }
        responses.push(text("final answer"));
        let (conversation, provider) = conversation(responses);
        let turn = conversation.run_turn(&[], "front", "go").await.unwrap();

        assert_eq!(turn.text, "final answer");
        assert_eq!(turn.active, "front");
        let handoffs = turn
            .entries
            .iter()
            .filter(|entry| matches!(entry.body, EntryBody::Handoff { .. }))
            .count();
        assert_eq!(handoffs, MAX_HANDOFFS_PER_TURN);
        let requests = provider.requests.lock().unwrap();
        let last_tools: Vec<&str> = requests[4].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            !last_tools
                .iter()
                .any(|name| name.starts_with(HANDOFF_PREFIX)),
            "{last_tools:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_agent_names_the_available_ones() {
        let (conversation, _) = conversation(Vec::new());
        let error = conversation
            .run_turn(&[], "nobody", "hi")
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no agent named 'nobody'"), "{message}");
        assert!(message.contains("helper"), "{message}");
    }
}
