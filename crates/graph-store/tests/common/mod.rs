use graph_core::store::{conversation, message_entries, Store, StoreError};
use graph_llm::types::ChatMessage;

pub trait MessageLog: Store {
    async fn append_messages(&self, id: &str, messages: &[ChatMessage]) -> Result<(), StoreError> {
        self.append_entries(id, &message_entries("chat", messages))
            .await
    }

    async fn load_messages(&self, id: &str) -> Result<Vec<ChatMessage>, StoreError> {
        Ok(conversation(&self.load_entries(id).await?))
    }
}

impl<T: Store + ?Sized> MessageLog for T {}
