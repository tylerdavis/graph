use super::models::{
    blocked, edit, expand_key, key_state, merge_value, section, set_optional, KeyState, Layers,
    Origin,
};
use crate::runtime::Workspace;
use crate::secrets::Secrets;
use anyhow::Result;
use dive_config::secrets::{InProject, SecretSource};
use dive_config::{FilesBackend, FilesConfig, StorageBackend};
use dive_core::files::BlobStore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use toml_edit::{value, Item, Table};

pub const ACCESS_KEY_NAME: &str = "DIVE_S3_ACCESS_KEY_ID";
pub const SECRET_KEY_NAME: &str = "DIVE_S3_SECRET_ACCESS_KEY";
pub const STORAGE_ENV: &str = "DIVE_STORAGE";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyEntry {
    pub value: Option<String>,
    pub names: Vec<String>,
    pub state: KeyState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilesEntry {
    pub backend: FilesBackend,
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub prefix: Option<String>,
    pub access_key_id: KeyEntry,
    pub secret_access_key: KeyEntry,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageView {
    pub file: PathBuf,
    pub global_file: PathBuf,
    pub global_workspace: bool,
    pub blocked: Option<String>,
    pub state: StorageBackend,
    pub state_origin: Option<Origin>,
    pub env_override: Option<String>,
    pub data_dir: Option<String>,
    pub data_dir_origin: Option<Origin>,
    pub files: Option<FilesEntry>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesEdit {
    pub backend: FilesBackend,
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub prefix: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageEdit {
    pub state: Option<StorageBackend>,
    pub data_dir: Option<String>,
    pub files: Option<FilesEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesProbe {
    pub ok: bool,
    pub millis: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsage {
    pub threads: u64,
    pub thread_bytes: u64,
    pub files: u64,
    pub file_bytes: u64,
    pub cache_bytes: u64,
}

struct Merged {
    state: Option<(toml::Value, Origin)>,
    data_dir: Option<(toml::Value, Origin)>,
    files: Option<(toml::Value, Origin)>,
    problems: Vec<String>,
}

fn read_layers(layers: &Layers) -> Merged {
    let mut merged = Merged {
        state: None,
        data_dir: None,
        files: None,
        problems: Vec::new(),
    };
    for (path, origin) in layers.read_paths() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(error) => {
                merged
                    .problems
                    .push(format!("parsing {}: {error}", path.display()));
                continue;
            }
        };
        let storage = table.get("storage").and_then(toml::Value::as_table);
        if let Some(state) = storage.and_then(|storage| storage.get("backend")) {
            merged.state = Some((state.clone(), origin));
        }
        if let Some(dir) = table
            .get("settings")
            .and_then(toml::Value::as_table)
            .and_then(|settings| settings.get("data_dir"))
        {
            merged.data_dir = Some((dir.clone(), origin));
        }
        if let Some(files) = storage.and_then(|storage| storage.get("files")) {
            match &mut merged.files {
                Some((existing, existing_origin)) => {
                    merge_value(existing, files.clone());
                    *existing_origin = origin;
                }
                None => merged.files = Some((files.clone(), origin)),
            }
        }
    }
    merged
}

fn key_entry(raw: Option<String>, resolver: &dyn SecretSource) -> KeyEntry {
    let (names, state) = key_state(raw.as_deref(), resolver);
    KeyEntry {
        value: raw,
        names,
        state,
    }
}

pub fn view(workspace: &Workspace, secrets: &Secrets) -> StorageView {
    let resolver = InProject::new(secrets.resolver(), crate::secrets::project_id(workspace));
    let mut view = view_layers(&Layers::of(workspace), &resolver);
    view.env_override = std::env::var(STORAGE_ENV)
        .ok()
        .filter(|value| !value.is_empty());
    view
}

pub fn view_layers(layers: &Layers, resolver: &dyn SecretSource) -> StorageView {
    let merged = read_layers(layers);
    let mut problems = merged.problems;
    let (state, state_origin) = match merged.state {
        Some((raw, origin)) => match raw.try_into::<StorageBackend>() {
            Ok(state) => (state, Some(origin)),
            Err(error) => {
                problems.push(format!("[storage].backend: {error}"));
                (StorageBackend::default(), Some(origin))
            }
        },
        None => (StorageBackend::default(), None),
    };
    let (data_dir, data_dir_origin) = match merged.data_dir {
        Some((toml::Value::String(dir), origin)) => (Some(dir), Some(origin)),
        Some((_, origin)) => {
            problems.push("[settings].data_dir must be a string".to_string());
            (None, Some(origin))
        }
        None => (None, None),
    };
    let files = merged
        .files
        .and_then(|(raw, origin)| match raw.try_into::<FilesConfig>() {
            Ok(files) => Some(FilesEntry {
                backend: files.backend,
                endpoint: files.endpoint,
                bucket: files.bucket,
                region: files.region,
                prefix: files.prefix,
                access_key_id: key_entry(files.access_key_id, resolver),
                secret_access_key: key_entry(files.secret_access_key, resolver),
                origin,
            }),
            Err(error) => {
                problems.push(format!("[storage.files]: {error}"));
                None
            }
        });
    StorageView {
        file: layers.file.clone(),
        global_file: layers.global.clone(),
        global_workspace: layers.global_workspace,
        blocked: blocked(&layers.file),
        state,
        state_origin,
        env_override: None,
        data_dir,
        data_dir_origin,
        files,
        problems,
    }
}

fn backend_name<T: Serialize>(backend: T) -> String {
    serde_json::to_value(backend)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn prune(doc: &mut toml_edit::DocumentMut, name: &str) {
    if doc
        .get(name)
        .and_then(Item::as_table)
        .is_some_and(Table::is_empty)
    {
        doc.remove(name);
    }
}

pub fn save(workspace: &Workspace, change: &StorageEdit) -> Result<()> {
    save_in(&Layers::of(workspace), change)
}

pub fn save_in(layers: &Layers, change: &StorageEdit) -> Result<()> {
    edit(layers, |doc| {
        let storage = section(doc, "storage")?;
        match change.state {
            Some(state) => {
                storage.insert("backend", value(backend_name(state)));
            }
            None => {
                storage.remove("backend");
            }
        }
        match &change.files {
            Some(files) => {
                let backend = files.backend;
                let mut entry = match storage.remove("files") {
                    Some(Item::Table(table)) => table,
                    Some(Item::Value(toml_edit::Value::InlineTable(inline))) => inline.into_table(),
                    _ => Table::new(),
                };
                entry.insert("backend", value(backend_name(backend)));
                let s3 = backend == FilesBackend::S3;
                fn field(text: &Option<String>, s3: bool) -> Option<&str> {
                    text.as_deref().filter(|_| s3)
                }
                set_optional(&mut entry, "endpoint", field(&files.endpoint, s3));
                set_optional(&mut entry, "bucket", field(&files.bucket, s3));
                set_optional(&mut entry, "region", field(&files.region, s3));
                set_optional(&mut entry, "prefix", field(&files.prefix, s3));
                set_optional(&mut entry, "access_key_id", field(&files.access_key_id, s3));
                set_optional(
                    &mut entry,
                    "secret_access_key",
                    field(&files.secret_access_key, s3),
                );
                storage.insert("files", Item::Table(entry));
            }
            None => {
                storage.remove("files");
            }
        }
        prune(doc, "storage");
        let settings = section(doc, "settings")?;
        set_optional(settings, "data_dir", change.data_dir.as_deref());
        prune(doc, "settings");
        Ok(())
    })
}

pub async fn probe(workspace: &Workspace, secrets: &Secrets, files: &FilesEdit) -> FilesProbe {
    let resolver = InProject::new(secrets.resolver(), crate::secrets::project_id(workspace));
    probe_with(files, &resolver).await
}

pub async fn probe_with(files: &FilesEdit, resolver: &dyn SecretSource) -> FilesProbe {
    let started = Instant::now();
    let finish = |error: Option<String>| FilesProbe {
        ok: error.is_none(),
        millis: started.elapsed().as_millis() as u64,
        error,
    };
    if files.backend != FilesBackend::S3 {
        return finish(None);
    }
    let expand = |text: &Option<String>| expand_key(text.as_deref(), resolver);
    let config = match (|| -> Result<FilesConfig, String> {
        Ok(FilesConfig {
            backend: FilesBackend::S3,
            endpoint: expand(&files.endpoint)?,
            bucket: expand(&files.bucket)?,
            region: expand(&files.region)?,
            prefix: expand(&files.prefix)?,
            access_key_id: expand(&files.access_key_id)?,
            secret_access_key: expand(&files.secret_access_key)?,
            missing_env: Vec::new(),
        })
    })() {
        Ok(config) => config,
        Err(error) => return finish(Some(error)),
    };
    let scratch = match tempfile::tempdir() {
        Ok(scratch) => scratch,
        Err(error) => return finish(Some(format!("creating a scratch folder: {error}"))),
    };
    let store = match dive_store::ObjectFiles::from_config(&config, scratch.path()) {
        Ok(store) => store,
        Err(error) => return finish(Some(error.0)),
    };
    let payload = format!("dive storage probe {}", uuid::Uuid::new_v4()).into_bytes();
    let result = async {
        let stored = store.put(payload.clone(), "text/plain").await?;
        let read = store.get(&stored.blob).await;
        store.delete(&stored.blob).await?;
        if read? != payload {
            return Err(dive_core::files::FileError(
                "the bucket returned different bytes than were written".to_string(),
            ));
        }
        Ok(())
    }
    .await;
    finish(result.err().map(|error| error.0))
}

fn walk(dir: &Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .fold((0, 0), |(count, bytes), entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                let (inner_count, inner_bytes) = walk(&entry.path());
                (count + inner_count, bytes + inner_bytes)
            }
            Ok(kind) if kind.is_file() => (
                count + 1,
                bytes + entry.metadata().map(|meta| meta.len()).unwrap_or(0),
            ),
            _ => (count, bytes),
        })
}

pub fn usage(data_dir: &Path) -> StorageUsage {
    let threads_dir = data_dir.join("threads");
    let threads = std::fs::read_dir(&threads_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .count() as u64
        })
        .unwrap_or(0);
    let (_, thread_bytes) = walk(&threads_dir);
    let (files, file_bytes) = walk(&dive_store::files_dir(data_dir));
    let (_, cache_bytes) = walk(&dive_store::cache_dir(data_dir));
    StorageUsage {
        threads,
        thread_bytes,
        files,
        file_bytes,
        cache_bytes,
    }
}

#[cfg(test)]
mod tests;
