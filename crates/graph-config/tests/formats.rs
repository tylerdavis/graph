use graph_config::{load_from, CONFIG_FORMAT, CONFIG_FORMAT_OLDEST};
use std::path::{Path, PathBuf};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture_dir(version: u32) -> PathBuf {
    fixtures_root().join(format!("v{version}"))
}

fn fixtures_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn every_format_version_has_a_fixture_directory() {
    for version in CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT {
        let dir = fixture_dir(version);
        assert!(
            dir.is_dir(),
            "config format {version} has no fixtures under {}",
            dir.display()
        );
        assert!(
            !fixtures_in(&dir).is_empty(),
            "config format {version} has an empty fixture directory"
        );
    }
}

#[test]
fn every_fixture_at_every_format_still_loads() {
    for version in CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT {
        for path in fixtures_in(&fixture_dir(version)) {
            let loaded = load_from(std::slice::from_ref(&path))
                .unwrap_or_else(|e| panic!("{} no longer loads: {e:#}", path.display()));
            let layer = &loaded.layers[0];
            assert_eq!(
                layer.format_version,
                version,
                "{} sits in the v{version} directory but declares format {}",
                path.display(),
                layer.format_version
            );
        }
    }
}

#[test]
fn golden_pairs_load_identically_across_formats() {
    for version in (CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT).filter(|v| *v < CONFIG_FORMAT) {
        for older in fixtures_in(&fixture_dir(version)) {
            let newer = fixture_dir(version + 1).join(older.file_name().unwrap());
            if !newer.exists() {
                continue;
            }
            let a =
                toml::to_string(&load_from(std::slice::from_ref(&older)).unwrap().config).unwrap();
            let b =
                toml::to_string(&load_from(std::slice::from_ref(&newer)).unwrap().config).unwrap();
            assert_eq!(
                a,
                b,
                "{} and {} must load to the same config",
                older.display(),
                newer.display()
            );
        }
    }
}

#[test]
fn a_newer_format_is_refused_before_the_schema_is_consulted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "format_version = {}\n[settings]\nkey_from_the_future = 1\n",
            CONFIG_FORMAT + 1
        ),
    )
    .unwrap();
    let err = format!("{:#}", load_from(std::slice::from_ref(&path)).unwrap_err());
    assert!(err.contains("is config format"), "{err}");
    assert!(
        err.contains(&format!("reads format {CONFIG_FORMAT}")),
        "{err}"
    );
    assert!(!err.contains("unknown field"), "{err}");
}

#[test]
fn layers_migrate_independently_before_they_merge() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    let project = dir.path().join("project.toml");
    std::fs::write(&global, "[settings]\nhistory_limit = 5\n").unwrap();
    std::fs::write(
        &project,
        format!("format_version = {CONFIG_FORMAT}\n[settings]\nhistory_limit = 9\n"),
    )
    .unwrap();
    let loaded = load_from(&[global, project]).unwrap();
    assert_eq!(loaded.config.settings.history_limit, 9);
    assert_eq!(loaded.layers.len(), 2);
    assert_eq!(loaded.layers[0].declared, None);
    assert_eq!(loaded.layers[0].format_version, CONFIG_FORMAT_OLDEST);
    assert_eq!(loaded.layers[1].declared, Some(CONFIG_FORMAT));
    assert!(loaded.layers[0].needs_migration());
    assert!(!loaded.layers[1].needs_migration());
}

#[test]
fn the_version_key_never_reaches_the_typed_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("format_version = {CONFIG_FORMAT}\n")).unwrap();
    let rendered = toml::to_string(&load_from(&[path]).unwrap().config).unwrap();
    assert!(!rendered.contains("format_version"), "{rendered}");
}
