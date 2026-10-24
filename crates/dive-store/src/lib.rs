//! Storage backends for runtime state: threads, messages, and the
//! observed-shape cache. `FileStore` (plain files, the default) and
//! `MemoryStore` (ephemeral, for CI) both implement `dive_core::Store`.

mod blobs;
mod file;
mod memory;
mod recording;

pub use blobs::{cache_dir, files_dir, ObjectFiles, S3Settings, FILES_DIR};
pub use file::{marker_format, FileStore, STORE_FORMAT, STORE_FORMAT_OLDEST};
pub use memory::MemoryStore;
pub use recording::RecordingRegistry;
