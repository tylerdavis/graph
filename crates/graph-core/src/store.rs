//! Persistence abstraction: threads, messages, and the observed-shape cache.
//! Implemented by graph-store; behind a trait so backends (file, memory,
//! future remote stores) can be swapped.

use async_trait::async_trait;
use graph_llm::types::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const USER_AUTHOR: &str = "user";

#[derive(Debug, Clone)]
pub struct ThreadMeta {
    pub id: String,
    pub title: String,
    /// Epoch milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
    pub owner: String,
    pub active: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadEntry {
    pub seq: u64,
    pub at: i64,
    pub author: String,
    #[serde(flatten)]
    pub body: EntryBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryBody {
    Message {
        message: ChatMessage,
    },
    Handoff {
        from: String,
        to: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
    },
    SubagentRun {
        agent: String,
        caller: String,
        input: Value,
        messages: Vec<ChatMessage>,
        output: Value,
        #[serde(rename = "final")]
        final_: bool,
    },
}

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub author: String,
    pub body: EntryBody,
}

impl NewEntry {
    pub fn message(author: &str, message: ChatMessage) -> Self {
        Self {
            author: author.to_string(),
            body: EntryBody::Message { message },
        }
    }
}

impl From<ThreadEntry> for NewEntry {
    fn from(entry: ThreadEntry) -> Self {
        Self {
            author: entry.author,
            body: entry.body,
        }
    }
}

pub fn message_entries(agent: &str, messages: &[ChatMessage]) -> Vec<NewEntry> {
    messages
        .iter()
        .map(|message| {
            let author = match message {
                ChatMessage::User { .. } => USER_AUTHOR,
                _ => agent,
            };
            NewEntry::message(author, message.clone())
        })
        .collect()
}

pub fn conversation(entries: &[ThreadEntry]) -> Vec<ChatMessage> {
    entries
        .iter()
        .filter_map(|entry| match &entry.body {
            EntryBody::Message { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// An observed (or declared) output shape for a tool.
#[derive(Debug, Clone)]
pub struct ToolShape {
    pub tool: String,
    pub schema: Value,
    pub example: Value,
    pub seen_count: i64,
}

#[derive(Debug, thiserror::Error)]
#[error("store error: {0}")]
pub struct StoreError(pub String);

#[async_trait]
pub trait Store: Send + Sync {
    async fn create_thread(&self, title: &str, owner: &str) -> Result<ThreadMeta, StoreError>;
    async fn get_thread(&self, id: &str) -> Result<Option<ThreadMeta>, StoreError>;
    /// Most recently updated thread, if any.
    async fn latest_thread(&self) -> Result<Option<ThreadMeta>, StoreError>;
    /// Newest first.
    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, StoreError>;
    async fn delete_thread(&self, id: &str) -> Result<bool, StoreError>;

    async fn append_entries(&self, thread_id: &str, entries: &[NewEntry])
        -> Result<(), StoreError>;
    async fn load_entries(&self, thread_id: &str) -> Result<Vec<ThreadEntry>, StoreError>;
    async fn set_active_agent(&self, thread_id: &str, agent: &str) -> Result<(), StoreError>;

    /// Record an observed output shape for a tool (upsert; bumps seen_count).
    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError>;
    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entries_serialize_flat_with_their_kind() {
        let entry = ThreadEntry {
            seq: 3,
            at: 10,
            author: "orchestrator".to_string(),
            body: EntryBody::Handoff {
                from: "orchestrator".to_string(),
                to: "plan_author".to_string(),
                message: "draft it".to_string(),
                via: None,
            },
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            value,
            json!({"seq": 3, "at": 10, "author": "orchestrator", "kind": "handoff",
                   "from": "orchestrator", "to": "plan_author", "message": "draft it"})
        );
        let back: ThreadEntry = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(back).unwrap(), value);

        let run = ThreadEntry {
            seq: 4,
            at: 11,
            author: "plan_author".to_string(),
            body: EntryBody::SubagentRun {
                agent: "plan_refiner".to_string(),
                caller: "plan_author".to_string(),
                input: json!({"goal": "g"}),
                messages: vec![ChatMessage::User {
                    content: "g".to_string(),
                }],
                output: json!({"changes": []}),
                final_: true,
            },
        };
        let value = serde_json::to_value(&run).unwrap();
        assert_eq!(value["kind"], json!("subagent_run"));
        assert_eq!(value["final"], json!(true));
        let back: ThreadEntry = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(back).unwrap(), value);
    }

    #[test]
    fn user_turns_are_authored_by_the_user_and_the_rest_by_the_agent() {
        let entries = message_entries(
            "chat",
            &[
                ChatMessage::User {
                    content: "hi".to_string(),
                },
                ChatMessage::Assistant {
                    content: Some("hello".to_string()),
                    thinking: Vec::new(),
                    tool_calls: Vec::new(),
                },
            ],
        );
        let authors: Vec<&str> = entries.iter().map(|e| e.author.as_str()).collect();
        assert_eq!(authors, [USER_AUTHOR, "chat"]);
    }
}
