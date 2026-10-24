use super::*;
use dive_config::CONFIG_FORMAT;
use dive_core::secrets::MemorySecrets;

const GLOBAL: &str = r#"version = 5

[settings]
data_dir = "~/dive-data"

[storage]
backend = "memory"

[storage.files]
backend = "s3"
bucket = "dive-files"
region = "us-east-1"
access_key_id = "${DIVE_S3_ACCESS_KEY_ID}"
secret_access_key = "${DIVE_S3_SECRET_ACCESS_KEY}"
"#;

const PROJECT: &str = r#"version = 5

# keep this comment
[storage.files]
prefix = "acme"
"#;

fn layers(global: &str, project: Option<&str>) -> (tempfile::TempDir, Layers) {
    let dir = tempfile::tempdir().unwrap();
    let global_path = dir.path().join("global").join("config.toml");
    std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
    std::fs::write(&global_path, global).unwrap();
    let file = dir.path().join("site").join(".dive").join("config.toml");
    if let Some(project) = project {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, project).unwrap();
    }
    let layers = Layers {
        load_paths: vec![global_path.clone(), file.clone()],
        global: global_path,
        file,
        global_workspace: false,
    };
    (dir, layers)
}

fn resolver() -> MemorySecrets {
    MemorySecrets::store().with(ACCESS_KEY_NAME, "AKIA-value")
}

fn text(layers: &Layers) -> String {
    std::fs::read_to_string(&layers.file).unwrap()
}

fn s3_edit() -> FilesEdit {
    FilesEdit {
        backend: FilesBackend::S3,
        endpoint: Some("https://s3.example.test".into()),
        bucket: Some("bucket".into()),
        region: Some("garage".into()),
        prefix: Some("site".into()),
        access_key_id: Some(format!("${{{ACCESS_KEY_NAME}}}")),
        secret_access_key: Some(format!("${{{SECRET_KEY_NAME}}}")),
    }
}

#[test]
fn nothing_configured_shows_the_defaults() {
    let (_dir, layers) = layers("version = 5\n", None);
    let view = view_layers(&layers, &resolver());
    assert_eq!(view.state, StorageBackend::File);
    assert_eq!(view.state_origin, None);
    assert_eq!(view.data_dir, None);
    assert_eq!(view.data_dir_origin, None);
    assert_eq!(view.files, None);
    assert_eq!(view.blocked, None);
    assert!(view.problems.is_empty(), "{:?}", view.problems);
}

#[test]
fn the_project_layer_merges_over_global_and_records_where_each_part_comes_from() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    let view = view_layers(&layers, &resolver());
    assert_eq!(view.state, StorageBackend::Memory);
    assert_eq!(view.state_origin, Some(Origin::Global));
    assert_eq!(view.data_dir.as_deref(), Some("~/dive-data"));
    assert_eq!(view.data_dir_origin, Some(Origin::Global));
    let files = view.files.unwrap();
    assert_eq!(files.backend, FilesBackend::S3);
    assert_eq!(files.bucket.as_deref(), Some("dive-files"));
    assert_eq!(files.prefix.as_deref(), Some("acme"));
    assert_eq!(files.origin, Origin::Workspace);
}

#[test]
fn key_fields_report_whether_their_secrets_are_set() {
    let (_dir, layers) = layers(GLOBAL, None);
    let files = view_layers(&layers, &resolver()).files.unwrap();
    assert_eq!(files.access_key_id.state, KeyState::Set);
    assert_eq!(files.access_key_id.names, vec![ACCESS_KEY_NAME.to_string()]);
    assert_eq!(files.secret_access_key.state, KeyState::Missing);
    let plain = GLOBAL.replace("${DIVE_S3_ACCESS_KEY_ID}", "AKIA-in-file");
    let (_dir, layers) = self::layers(&plain, None);
    let files = view_layers(&layers, &resolver()).files.unwrap();
    assert_eq!(files.access_key_id.state, KeyState::InFile);
    assert!(files.access_key_id.names.is_empty());
}

#[test]
fn saving_writes_the_project_layer_and_keeps_its_comments() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    save_in(
        &layers,
        &StorageEdit {
            state: Some(StorageBackend::File),
            data_dir: Some("~/elsewhere".into()),
            files: Some(s3_edit()),
        },
    )
    .unwrap();
    let written = text(&layers);
    assert!(written.contains("# keep this comment"), "{written}");
    assert!(written.contains("backend = \"file\""), "{written}");
    let view = view_layers(&layers, &resolver());
    assert_eq!(view.state, StorageBackend::File);
    assert_eq!(view.state_origin, Some(Origin::Workspace));
    assert_eq!(view.data_dir.as_deref(), Some("~/elsewhere"));
    let files = view.files.unwrap();
    assert_eq!(files.endpoint.as_deref(), Some("https://s3.example.test"));
    assert_eq!(files.prefix.as_deref(), Some("site"));
    assert_eq!(
        files.secret_access_key.value.as_deref(),
        Some("${DIVE_S3_SECRET_ACCESS_KEY}")
    );
}

#[test]
fn a_local_backend_drops_the_s3_fields() {
    let (_dir, layers) = layers("version = 5\n", Some(GLOBAL));
    let mut files = s3_edit();
    files.backend = FilesBackend::File;
    save_in(
        &layers,
        &StorageEdit {
            files: Some(files),
            ..StorageEdit::default()
        },
    )
    .unwrap();
    let written = text(&layers);
    assert!(!written.contains("bucket"), "{written}");
    assert!(!written.contains("access_key_id"), "{written}");
    let files = view_layers(&layers, &resolver()).files.unwrap();
    assert_eq!(files.backend, FilesBackend::File);
}

#[test]
fn clearing_every_choice_removes_the_sections_from_the_file() {
    let (_dir, layers) = layers("version = 5\n", Some(GLOBAL));
    save_in(&layers, &StorageEdit::default()).unwrap();
    let written = text(&layers);
    assert!(!written.contains("[storage"), "{written}");
    assert!(!written.contains("[settings"), "{written}");
    let view = view_layers(&layers, &resolver());
    assert_eq!(view.state_origin, None);
    assert_eq!(view.files, None);
}

#[test]
fn saving_creates_a_missing_project_file() {
    let (_dir, layers) = layers("version = 5\n", None);
    save_in(
        &layers,
        &StorageEdit {
            state: Some(StorageBackend::Memory),
            ..StorageEdit::default()
        },
    )
    .unwrap();
    assert!(
        text(&layers).starts_with("version = 5"),
        "{}",
        text(&layers)
    );
    assert_eq!(
        view_layers(&layers, &resolver()).state,
        StorageBackend::Memory
    );
}

#[test]
fn a_newer_file_is_shown_but_never_edited() {
    let newer = format!("version = {}\n", CONFIG_FORMAT + 1);
    let (_dir, layers) = layers(GLOBAL, Some(&newer));
    assert!(view_layers(&layers, &resolver()).blocked.is_some());
    assert!(save_in(&layers, &StorageEdit::default()).is_err());
    assert_eq!(text(&layers), newer);
}

#[test]
fn usage_counts_threads_files_and_cache() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(usage(dir.path()), StorageUsage::default());
    for (path, bytes) in [
        ("threads/a/meta.json", 10),
        ("threads/a/entries.jsonl", 30),
        ("threads/b/meta.json", 5),
        ("files/one.png", 100),
        ("files/two.png", 50),
        ("cache/files/three.png", 7),
    ] {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0u8; bytes]).unwrap();
    }
    assert_eq!(
        usage(dir.path()),
        StorageUsage {
            threads: 2,
            thread_bytes: 45,
            files: 2,
            file_bytes: 150,
            cache_bytes: 7,
        }
    );
}

#[tokio::test]
async fn local_backends_need_no_probe() {
    for backend in [FilesBackend::File, FilesBackend::Memory] {
        let probe = probe_with(
            &FilesEdit {
                backend,
                ..s3_edit()
            },
            &resolver(),
        )
        .await;
        assert!(probe.ok, "{probe:?}");
    }
}

#[tokio::test]
async fn a_probe_names_an_unset_secret() {
    let probe = probe_with(&s3_edit(), &resolver()).await;
    assert!(!probe.ok);
    assert!(
        probe.error.as_deref().unwrap().contains(SECRET_KEY_NAME),
        "{probe:?}"
    );
}

#[tokio::test]
async fn a_probe_writes_reads_and_deletes_in_a_real_bucket() {
    let var = |name: &str| std::env::var(format!("DIVE_TEST_S3_{name}")).ok();
    let (Some(bucket), Some(key), Some(secret)) = (var("BUCKET"), var("KEY_ID"), var("SECRET"))
    else {
        eprintln!("DIVE_TEST_S3_* unset; skipping the S3 probe");
        return;
    };
    let secrets = MemorySecrets::store()
        .with(ACCESS_KEY_NAME, &key)
        .with(SECRET_KEY_NAME, &secret);
    let edit = FilesEdit {
        endpoint: var("ENDPOINT"),
        bucket: Some(bucket),
        region: var("REGION"),
        prefix: Some(format!("ci/{}", uuid::Uuid::new_v4().simple())),
        ..s3_edit()
    };
    let probe = probe_with(&edit, &secrets).await;
    assert!(probe.ok, "{probe:?}");
}
