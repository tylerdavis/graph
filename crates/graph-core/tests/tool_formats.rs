use graph_core::format::{TOOL_FORMAT, TOOL_FORMAT_OLDEST};
use graph_core::user_tools::{parse_tool_source, validate_tool};
use std::path::{Path, PathBuf};

fn fixture_dir(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/tools/v{version}"))
}

fn fixtures_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    paths.sort();
    paths
}

fn declared(path: &Path) -> u64 {
    let raw = std::fs::read_to_string(path).unwrap();
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
    value["version"].as_u64().unwrap_or(1)
}

fn load(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap();
    let doc = parse_tool_source(&raw)
        .unwrap_or_else(|e| panic!("{} no longer loads: {e}", path.display()));
    validate_tool(&doc).unwrap_or_else(|e| panic!("{} no longer validates: {e}", path.display()));
    serde_json::to_value(&doc).unwrap()
}

#[test]
fn every_tool_version_has_a_fixture_directory() {
    for version in TOOL_FORMAT_OLDEST..=TOOL_FORMAT {
        let dir = fixture_dir(version);
        assert!(
            dir.is_dir(),
            "tool format {version} has no fixtures under {}",
            dir.display()
        );
        assert!(
            !fixtures_in(&dir).is_empty(),
            "tool format {version} has an empty fixture directory"
        );
    }
}

#[test]
fn every_tool_fixture_at_every_format_still_loads() {
    for version in TOOL_FORMAT_OLDEST..=TOOL_FORMAT {
        for path in fixtures_in(&fixture_dir(version)) {
            assert_eq!(
                declared(&path),
                u64::from(version),
                "{} sits in the v{version} directory but declares another version",
                path.display()
            );
            load(&path);
        }
    }
}

#[test]
fn tool_golden_pairs_load_identically_across_formats() {
    for version in (TOOL_FORMAT_OLDEST..TOOL_FORMAT).filter(|v| *v < TOOL_FORMAT) {
        for older in fixtures_in(&fixture_dir(version)) {
            let newer = fixture_dir(version + 1).join(older.file_name().unwrap());
            if !newer.exists() {
                continue;
            }
            assert_eq!(
                load(&older),
                load(&newer),
                "{} and {} must load to the same tool",
                older.display(),
                newer.display()
            );
        }
    }
}

#[test]
fn a_newer_tool_format_is_refused_before_the_schema_is_consulted() {
    let raw = format!(
        "version: {}\nname: future\ndescription: d\nkind: reshape\nkey_from_the_future: 1\n",
        TOOL_FORMAT + 1
    );
    let err = parse_tool_source(&raw).unwrap_err().to_string();
    assert!(
        err.contains(&format!("tool version {}", TOOL_FORMAT + 1)),
        "{err}"
    );
    assert!(!err.contains("unknown field"), "{err}");
}
