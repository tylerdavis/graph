//! In-memory `Store`: ephemeral runtime state for CI jobs and tests.
//! Nothing survives the process.

use graph_core::store::{Store, StoreError, ThreadMeta, ToolShape};
use graph_llm::types::ChatMessage;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    threads: HashMap<String, (ThreadMeta, Vec<ChatMessage>)>,
    shapes: HashMap<String, ToolShape>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[async_trait::async_trait]
impl Store for MemoryStore {
    async fn create_thread(&self, title: &str) -> Result<ThreadMeta, StoreError> {
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let now = now_ms();
        let meta = ThreadMeta {
            id: id.clone(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        };
        self.inner
            .lock()
            .unwrap()
            .threads
            .insert(id, (meta.clone(), Vec::new()));
        Ok(meta)
    }

    async fn get_thread(&self, id: &str) -> Result<Option<ThreadMeta>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .threads
            .get(id)
            .map(|(meta, _)| meta.clone()))
    }

    async fn latest_thread(&self) -> Result<Option<ThreadMeta>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .threads
            .values()
            .max_by_key(|(meta, _)| meta.updated_at)
            .map(|(meta, _)| meta.clone()))
    }

    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, StoreError> {
        let mut threads: Vec<ThreadMeta> = self
            .inner
            .lock()
            .unwrap()
            .threads
            .values()
            .map(|(meta, _)| meta.clone())
            .collect();
        threads.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        Ok(threads)
    }

    async fn delete_thread(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.inner.lock().unwrap().threads.remove(id).is_some())
    }

    async fn append_messages(
        &self,
        thread_id: &str,
        messages: &[ChatMessage],
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let (meta, stored) = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
        stored.extend(messages.iter().cloned());
        meta.message_count = stored.len() as i64;
        meta.updated_at = now_ms();
        Ok(())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Vec<ChatMessage>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .threads
            .get(thread_id)
            .map(|(_, messages)| messages.clone())
            .unwrap_or_default())
    }

    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.shapes.entry(tool.to_string()).or_insert(ToolShape {
            tool: tool.to_string(),
            schema: schema.clone(),
            example: example.clone(),
            seen_count: 0,
        });
        entry.schema = schema.clone();
        entry.example = example.clone();
        entry.seen_count += 1;
        Ok(())
    }

    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .shapes
            .values()
            .cloned()
            .collect())
    }
}
