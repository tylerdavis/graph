use async_trait::async_trait;
use dive_core::files::{BlobRef, BlobStore, FileError, FileLocation, StoredFile};
use dive_store::ObjectFiles;
use std::path::PathBuf;
use std::sync::Arc;

struct Unavailable(String);

#[async_trait]
impl BlobStore for Unavailable {
    async fn put(&self, _bytes: Vec<u8>, _mime: &str) -> Result<StoredFile, FileError> {
        Err(FileError(self.0.clone()))
    }

    async fn get(&self, _blob: &BlobRef) -> Result<Vec<u8>, FileError> {
        Err(FileError(self.0.clone()))
    }

    async fn delete(&self, _blob: &BlobRef) -> Result<(), FileError> {
        Err(FileError(self.0.clone()))
    }

    async fn locate(&self, _blob: &BlobRef) -> Result<FileLocation, FileError> {
        Err(FileError(self.0.clone()))
    }

    async fn materialize(&self, _blob: &BlobRef) -> Result<PathBuf, FileError> {
        Err(FileError(self.0.clone()))
    }
}

pub(crate) fn open(
    config: &dive_config::Config,
    backend: dive_config::StorageBackend,
) -> Arc<dyn BlobStore> {
    let data_dir = dive_config::expand_tilde(&config.settings.data_dir);
    let opened = match (&config.storage.files, backend) {
        (Some(files), _) => ObjectFiles::from_config(files, &data_dir),
        (None, dive_config::StorageBackend::File) => {
            ObjectFiles::local(&dive_store::files_dir(&data_dir))
        }
        (None, dive_config::StorageBackend::Memory) => ObjectFiles::memory(),
    };
    match opened {
        Ok(files) => Arc::new(files),
        Err(e) => {
            tracing::warn!(error = %e, "file storage is unavailable");
            Arc::new(Unavailable(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_storage_follows_the_state_backend_unless_configured() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = dive_config::Config::default();
        config.settings.data_dir = dir.path().to_path_buf();
        let on_disk = open(&config, dive_config::StorageBackend::File);
        let stored = on_disk.put(b"hi".to_vec(), "text/plain").await.unwrap();
        assert_eq!(
            on_disk.locate(&stored.blob).await.unwrap(),
            FileLocation::Path(dir.path().join("files").join(stored.blob.file_name()))
        );

        let in_memory = open(&config, dive_config::StorageBackend::Memory);
        let stored = in_memory.put(b"hi".to_vec(), "text/plain").await.unwrap();
        assert!(!dir
            .path()
            .join("files")
            .join(format!("{}.bin", stored.blob.hash()))
            .exists());
        assert_eq!(in_memory.get(&stored.blob).await.unwrap(), b"hi");
    }

    #[tokio::test]
    async fn a_broken_file_store_loads_and_fails_only_when_used() {
        let mut config = dive_config::Config::default();
        config.storage.files = Some(dive_config::FilesConfig {
            backend: dive_config::FilesBackend::S3,
            endpoint: None,
            bucket: None,
            region: None,
            prefix: None,
            access_key_id: None,
            secret_access_key: None,
            missing_env: Vec::new(),
        });
        let files = open(&config, dive_config::StorageBackend::File);
        let err = files.put(b"x".to_vec(), "text/plain").await.unwrap_err();
        assert!(err.to_string().contains("needs a bucket"), "{err}");
    }
}
