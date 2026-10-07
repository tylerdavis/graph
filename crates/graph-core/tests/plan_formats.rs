use graph_core::format::{PLAN_FORMAT, PLAN_FORMAT_OLDEST};
use graph_core::pipeline::doc::parse_plan_source;
use std::path::{Path, PathBuf};

fn fixture_dir(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/plans/v{version}"))
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
    let doc = parse_plan_source(&raw, &path.display().to_string())
        .unwrap_or_else(|e| panic!("{} no longer loads: {e}", path.display()));
    serde_json::to_value(&doc).unwrap()
}

#[test]
fn every_plan_version_has_a_fixture_directory() {
    for version in PLAN_FORMAT_OLDEST..=PLAN_FORMAT {
        let dir = fixture_dir(version);
        assert!(
            dir.is_dir(),
            "plan format {version} has no fixtures under {}",
            dir.display()
        );
        assert!(
            !fixtures_in(&dir).is_empty(),
            "plan format {version} has an empty fixture directory"
        );
    }
}

#[test]
fn every_plan_fixture_at_every_format_still_loads() {
    for version in PLAN_FORMAT_OLDEST..=PLAN_FORMAT {
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
fn plan_golden_pairs_load_identically_across_formats() {
    for version in (PLAN_FORMAT_OLDEST..PLAN_FORMAT).filter(|v| *v < PLAN_FORMAT) {
        for older in fixtures_in(&fixture_dir(version)) {
            let newer = fixture_dir(version + 1).join(older.file_name().unwrap());
            if !newer.exists() {
                continue;
            }
            assert_eq!(
                load(&older),
                load(&newer),
                "{} and {} must load to the same plan",
                older.display(),
                newer.display()
            );
        }
    }
}

#[test]
fn a_newer_plan_format_is_refused_before_the_schema_is_consulted() {
    let raw = format!(
        "version: {}\nidentifier: future\nname: Future\ndescription: d\nsteps: []\nkey_from_the_future: 1\n",
        PLAN_FORMAT + 1
    );
    let err = parse_plan_source(&raw, "future.yaml")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!("plan version {}", PLAN_FORMAT + 1)),
        "{err}"
    );
    assert!(!err.contains("unknown field"), "{err}");
}
