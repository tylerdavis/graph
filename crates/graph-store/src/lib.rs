//! Storage backends for runtime state: threads, messages, and the
//! observed-shape cache. `FileStore` (plain files, the default) and
//! `MemoryStore` (ephemeral, for CI) both implement `graph_core::Store`.

mod file;
mod memory;
mod recording;

pub use file::{marker_format, FileStore, STORE_FORMAT, STORE_FORMAT_OLDEST};
pub use memory::MemoryStore;
pub use recording::RecordingRegistry;
