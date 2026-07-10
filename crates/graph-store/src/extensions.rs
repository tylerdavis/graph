//! Bundled lbug extensions (FTS, VECTOR).
//!
//! The extension binaries are embedded at compile time (fetched by
//! `build.rs`, pinned to the engine's expected version) and loaded by
//! *path*: a `LOAD EXTENSION '<file>'` whose argument is not an official
//! extension name dlopens the file directly — no `INSTALL`, no network at
//! runtime, no dependency on extension.ladybugdb.com being up. dlopen
//! needs a real file, so the bytes materialize under the store's own
//! directory on first use.

use crate::db::GraphStore;
use graph_core::store::StoreError;
use std::path::{Path, PathBuf};

/// Matches the engine's expected extension version (set by build.rs).
pub const EXTENSION_VERSION: &str = env!("GRAPH_LBUG_EXT_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extension {
    Fts,
    Vector,
}

impl Extension {
    pub fn name(self) -> &'static str {
        match self {
            Extension::Fts => "fts",
            Extension::Vector => "vector",
        }
    }

    fn bytes(self) -> &'static [u8] {
        match self {
            Extension::Fts => include_bytes!(env!("GRAPH_LBUG_EXT_FTS")),
            Extension::Vector => include_bytes!(env!("GRAPH_LBUG_EXT_VECTOR")),
        }
    }
}

/// Write `ext`'s embedded binary under `base_dir` (idempotent, versioned
/// path) and return the file path to `LOAD EXTENSION` from.
pub fn materialize(ext: Extension, base_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = base_dir.join("extensions").join(EXTENSION_VERSION);
    let path = dir.join(format!("lib{}.lbug_extension", ext.name()));
    if !path.exists() {
        std::fs::create_dir_all(&dir)?;
        // Write-then-rename: a concurrent or aborted writer must never
        // leave a truncated file at the final path for dlopen to trip on.
        let tmp = dir.join(format!(".lib{}.lbug_extension.tmp", ext.name()));
        std::fs::write(&tmp, ext.bytes())?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

impl GraphStore {
    /// Load a bundled extension into this database (idempotent — lbug
    /// ignores a re-load of an already-loaded extension).
    pub async fn load_extension(&self, ext: Extension) -> Result<(), StoreError> {
        // The database path is a file; extensions live beside it
        // (`<data_dir>/extensions/<version>/` in the default layout).
        let base = self.dir.parent().unwrap_or(&self.dir);
        let path = materialize(ext, base)
            .map_err(|e| StoreError(format!("materializing {} extension: {e}", ext.name())))?;
        // Single-quoted Cypher string literal; escape quotes in the path.
        let literal = path.display().to_string().replace('\'', "''");
        self.exec(move |conn| {
            conn.query(&format!("LOAD EXTENSION '{literal}'"))?;
            Ok(())
        })
        .await
    }
}
