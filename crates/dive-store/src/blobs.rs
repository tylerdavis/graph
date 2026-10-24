use async_trait::async_trait;
use dive_config::{describe_missing_env, FilesBackend, FilesConfig};
use dive_core::files::{BlobRef, BlobStore, FileError, FileLocation, StoredFile};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::signer::{Method, Signer};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SIGNED_URL_LIFETIME: Duration = Duration::from_secs(3600);

const CACHE_LIMIT: u64 = 512 * 1024 * 1024;

pub const FILES_DIR: &str = "files";

pub fn files_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(FILES_DIR)
}

pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join(FILES_DIR)
}

pub struct ObjectFiles {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    local_root: Option<PathBuf>,
    signer: Option<Arc<AmazonS3>>,
    cache: PathBuf,
    cache_limit: u64,
    _scratch: Option<tempfile::TempDir>,
}

#[derive(Debug, Clone, Default)]
pub struct S3Settings {
    pub endpoint: Option<String>,
    pub bucket: String,
    pub region: Option<String>,
    pub prefix: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

fn failed(what: &str, e: impl std::fmt::Display) -> FileError {
    FileError(format!("{what}: {e}"))
}

impl ObjectFiles {
    pub fn local(root: &Path) -> Result<Self, FileError> {
        std::fs::create_dir_all(root)
            .map_err(|e| failed(&format!("creating {}", root.display()), e))?;
        let store = LocalFileSystem::new_with_prefix(root)
            .map_err(|e| failed(&format!("opening {}", root.display()), e))?;
        Ok(Self {
            store: Arc::new(store),
            prefix: String::new(),
            local_root: Some(root.to_path_buf()),
            signer: None,
            cache: root.to_path_buf(),
            cache_limit: CACHE_LIMIT,
            _scratch: None,
        })
    }

    pub fn memory() -> Result<Self, FileError> {
        let scratch = tempfile::tempdir().map_err(|e| failed("creating a file cache", e))?;
        Ok(Self {
            store: Arc::new(InMemory::new()),
            prefix: String::new(),
            local_root: None,
            signer: None,
            cache: scratch.path().to_path_buf(),
            cache_limit: CACHE_LIMIT,
            _scratch: Some(scratch),
        })
    }

    pub fn s3(settings: S3Settings, cache: &Path) -> Result<Self, FileError> {
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(&settings.bucket);
        if let Some(endpoint) = &settings.endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"));
        }
        if let Some(region) = &settings.region {
            builder = builder.with_region(region);
        }
        if let Some(key) = &settings.access_key_id {
            builder = builder.with_access_key_id(key);
        }
        if let Some(secret) = &settings.secret_access_key {
            builder = builder.with_secret_access_key(secret);
        }
        let s3 = Arc::new(
            builder
                .build()
                .map_err(|e| failed(&format!("opening bucket {}", settings.bucket), e))?,
        );
        Ok(Self {
            store: s3.clone(),
            prefix: settings
                .prefix
                .map(|prefix| prefix.trim_matches('/').to_string())
                .unwrap_or_default(),
            local_root: None,
            signer: Some(s3),
            cache: cache.to_path_buf(),
            cache_limit: CACHE_LIMIT,
            _scratch: None,
        })
    }

    pub fn from_config(config: &FilesConfig, data_dir: &Path) -> Result<Self, FileError> {
        if !config.missing_env.is_empty() {
            return Err(FileError(format!(
                "file storage cannot open: {}",
                describe_missing_env("storage.files", &config.missing_env)
            )));
        }
        match config.backend {
            FilesBackend::File => Self::local(&files_dir(data_dir)),
            FilesBackend::Memory => Self::memory(),
            FilesBackend::S3 => {
                let bucket = config.bucket.clone().ok_or_else(|| {
                    FileError("[storage.files] backend = \"s3\" needs a bucket".to_string())
                })?;
                Self::s3(
                    S3Settings {
                        endpoint: config.endpoint.clone(),
                        bucket,
                        region: config.region.clone(),
                        prefix: config.prefix.clone(),
                        access_key_id: config.access_key_id.clone(),
                        secret_access_key: config.secret_access_key.clone(),
                    },
                    &cache_dir(data_dir),
                )
            }
        }
    }

    pub fn with_cache_limit(mut self, bytes: u64) -> Self {
        self.cache_limit = bytes;
        self
    }

    fn key(&self, blob: &BlobRef) -> ObjectPath {
        if self.local_root.is_some() {
            return ObjectPath::from(blob.file_name());
        }
        let name = format!("{FILES_DIR}/{}", blob.file_name());
        if self.prefix.is_empty() {
            ObjectPath::from(name)
        } else {
            ObjectPath::from(format!("{}/{name}", self.prefix))
        }
    }

    fn local_path(&self, blob: &BlobRef) -> Option<PathBuf> {
        self.local_root
            .as_ref()
            .map(|root| root.join(blob.file_name()))
    }
}

fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().append(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

fn trim_cache(dir: &Path, limit: u64, keep: &Path) -> std::io::Result<()> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_file() {
            files.push((meta.modified()?, meta.len(), entry.path()));
        }
    }
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= limit {
            break;
        }
        if path != keep && std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
    Ok(())
}

fn dimensions(mime: &str, bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    if !mime.starts_with("image/") {
        return (None, None);
    }
    match imagesize::blob_size(bytes) {
        Ok(size) => (
            u32::try_from(size.width).ok(),
            u32::try_from(size.height).ok(),
        ),
        Err(_) => (None, None),
    }
}

#[async_trait]
impl BlobStore for ObjectFiles {
    async fn put(&self, bytes: Vec<u8>, mime: &str) -> Result<StoredFile, FileError> {
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let blob = BlobRef::new(hash, mime);
        let key = self.key(&blob);
        let (width, height) = dimensions(blob.mime(), &bytes);
        let size = bytes.len() as u64;
        match self.store.head(&key).await {
            Ok(_) => {}
            Err(object_store::Error::NotFound { .. }) => {
                self.store
                    .put(&key, PutPayload::from(bytes))
                    .await
                    .map_err(|e| failed(&format!("storing {blob}"), e))?;
            }
            Err(e) => return Err(failed(&format!("checking {blob}"), e)),
        }
        Ok(StoredFile {
            mime: blob.mime().to_string(),
            blob,
            bytes: size,
            width,
            height,
        })
    }

    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, FileError> {
        let found = self
            .store
            .get(&self.key(blob))
            .await
            .map_err(|e| failed(&format!("reading {blob}"), e))?;
        let bytes = found
            .bytes()
            .await
            .map_err(|e| failed(&format!("reading {blob}"), e))?;
        Ok(bytes.to_vec())
    }

    async fn delete(&self, blob: &BlobRef) -> Result<(), FileError> {
        match self.store.delete(&self.key(blob)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(failed(&format!("deleting {blob}"), e)),
        }
        if self.local_root.is_none() {
            let _ = std::fs::remove_file(self.cache.join(blob.file_name()));
        }
        Ok(())
    }

    async fn locate(&self, blob: &BlobRef) -> Result<FileLocation, FileError> {
        if let Some(path) = self.local_path(blob) {
            return Ok(FileLocation::Path(path));
        }
        if let Some(signer) = &self.signer {
            let url = signer
                .signed_url(Method::GET, &self.key(blob), SIGNED_URL_LIFETIME)
                .await
                .map_err(|e| failed(&format!("signing a link to {blob}"), e))?;
            return Ok(FileLocation::Url(url.to_string()));
        }
        Ok(FileLocation::Path(self.materialize(blob).await?))
    }

    fn shared(&self) -> bool {
        self.signer.is_some()
    }

    async fn materialize(&self, blob: &BlobRef) -> Result<PathBuf, FileError> {
        if let Some(path) = self.local_path(blob) {
            return if path.is_file() {
                Ok(path)
            } else {
                Err(FileError(format!("no file stored for {blob}")))
            };
        }
        let path = self.cache.join(blob.file_name());
        if path.is_file() {
            touch(&path);
            return Ok(path);
        }
        let bytes = self.get(blob).await?;
        let cache = self.cache.clone();
        let target = path.clone();
        let limit = self.cache_limit;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&cache)?;
            let mut tmp = tempfile::NamedTempFile::new_in(&cache)?;
            tmp.write_all(&bytes)?;
            tmp.persist(&target).map_err(|e| e.error)?;
            if let Err(e) = trim_cache(&cache, limit, &target) {
                tracing::warn!(error = %e, "couldn't trim the file cache");
            }
            Ok(())
        })
        .await
        .map_err(|e| failed("caching a file", e))?
        .map_err(|e| failed(&format!("caching {blob} at {}", path.display()), e))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x9d,
        0x74, 0x66, 0x1a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60,
        0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0x7a, 0x5e, 0xab, 0x3f, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    async fn round_trip(files: &ObjectFiles) {
        let stored = files.put(PNG_1X1.to_vec(), "image/png").await.unwrap();
        assert_eq!(stored.mime, "image/png");
        assert_eq!(stored.bytes, PNG_1X1.len() as u64);
        assert_eq!((stored.width, stored.height), (Some(3), Some(2)));
        assert!(stored.blob.to_string().starts_with("blob:sha256-"));
        assert!(stored.blob.to_string().ends_with(".png"));

        let again = files.put(PNG_1X1.to_vec(), "image/png").await.unwrap();
        assert_eq!(again, stored);

        assert_eq!(files.get(&stored.blob).await.unwrap(), PNG_1X1);
        let path = files.materialize(&stored.blob).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), PNG_1X1);
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            stored.blob.file_name()
        );

        files.delete(&stored.blob).await.unwrap();
        assert!(files.get(&stored.blob).await.is_err());
        files.delete(&stored.blob).await.unwrap();
    }

    #[tokio::test]
    async fn the_download_cache_drops_the_least_recently_used_files_past_its_limit() {
        let files = ObjectFiles::memory().unwrap().with_cache_limit(10);
        let first = files
            .put(b"aaaaaa".to_vec(), "image/png")
            .await
            .unwrap()
            .blob;
        let second = files
            .put(b"bbbbbb".to_vec(), "image/png")
            .await
            .unwrap()
            .blob;
        let first_path = files.materialize(&first).await.unwrap();
        std::fs::File::options()
            .append(true)
            .open(&first_path)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let second_path = files.materialize(&second).await.unwrap();
        assert!(!first_path.exists());
        assert!(second_path.exists());

        assert_eq!(files.materialize(&first).await.unwrap(), first_path);
        assert!(first_path.exists());
        files.delete(&first).await.unwrap();
        assert!(!first_path.exists());
    }

    #[tokio::test]
    async fn files_on_disk_round_trip_and_are_located_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let files = ObjectFiles::local(&files_dir(dir.path())).unwrap();
        let stored = files.put(PNG_1X1.to_vec(), "image/png").await.unwrap();
        let expected = dir.path().join("files").join(stored.blob.file_name());
        assert_eq!(
            files.locate(&stored.blob).await.unwrap(),
            FileLocation::Path(expected.clone())
        );
        assert_eq!(files.materialize(&stored.blob).await.unwrap(), expected);
        round_trip(&files).await;
        assert!(files.materialize(&stored.blob).await.is_err());
    }

    #[tokio::test]
    async fn files_in_memory_round_trip_through_a_cache_on_disk() {
        let files = ObjectFiles::memory().unwrap();
        let stored = files.put(PNG_1X1.to_vec(), "image/png").await.unwrap();
        match files.locate(&stored.blob).await.unwrap() {
            FileLocation::Path(path) => assert_eq!(std::fs::read(path).unwrap(), PNG_1X1),
            other => panic!("expected a path, got {other:?}"),
        }
        round_trip(&files).await;
    }

    #[tokio::test]
    async fn non_images_have_no_dimensions() {
        let files = ObjectFiles::memory().unwrap();
        let stored = files.put(b"hello".to_vec(), "text/plain").await.unwrap();
        assert_eq!(stored.blob.extension(), "txt");
        assert_eq!((stored.width, stored.height), (None, None));
    }

    #[test]
    fn an_s3_backend_without_a_bucket_or_with_an_unset_secret_refuses_to_open() {
        let data = tempfile::tempdir().unwrap();
        let mut config = FilesConfig {
            backend: FilesBackend::S3,
            endpoint: None,
            bucket: None,
            region: None,
            prefix: None,
            access_key_id: None,
            secret_access_key: None,
            missing_env: Vec::new(),
        };
        let err = ObjectFiles::from_config(&config, data.path())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("needs a bucket"), "{err}");
        config.bucket = Some("b".into());
        config.missing_env.push(dive_config::MissingEnv {
            field: "secret_access_key".into(),
            var: "S3_SECRET".into(),
        });
        let err = ObjectFiles::from_config(&config, data.path())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("S3_SECRET"), "{err}");
    }

    fn s3_settings_from_env() -> Option<S3Settings> {
        let var = |name: &str| std::env::var(format!("DIVE_TEST_S3_{name}")).ok();
        Some(S3Settings {
            endpoint: var("ENDPOINT"),
            bucket: var("BUCKET")?,
            region: var("REGION"),
            prefix: Some(format!("ci/{}", uuid::Uuid::new_v4().simple())),
            access_key_id: Some(var("KEY_ID")?),
            secret_access_key: Some(var("SECRET")?),
        })
    }

    #[tokio::test]
    async fn files_in_s3_round_trip_and_are_located_by_a_signed_link() {
        let Some(settings) = s3_settings_from_env() else {
            eprintln!("DIVE_TEST_S3_* unset; skipping the S3 round trip");
            return;
        };
        let cache = tempfile::tempdir().unwrap();
        let files = ObjectFiles::s3(settings, cache.path()).unwrap();
        let stored = files.put(PNG_1X1.to_vec(), "image/png").await.unwrap();
        let url = match files.locate(&stored.blob).await.unwrap() {
            FileLocation::Url(url) => url,
            other => panic!("expected a signed link, got {other:?}"),
        };
        assert!(
            url.contains(&format!("/files/{}", stored.blob.file_name())),
            "{url}"
        );
        let fetched = reqwest::get(&url).await.unwrap();
        assert!(fetched.status().is_success(), "{}", fetched.status());
        assert_eq!(fetched.bytes().await.unwrap().as_ref(), PNG_1X1);
        assert_eq!(
            files.materialize(&stored.blob).await.unwrap(),
            cache.path().join(stored.blob.file_name())
        );
        round_trip(&files).await;
    }
}
