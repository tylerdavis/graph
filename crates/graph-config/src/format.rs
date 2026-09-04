use std::fmt;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Value};

pub const CONFIG_FORMAT: u32 = 2;

pub const CONFIG_FORMAT_OLDEST: u32 = 1;

pub const FORMAT_KEY: &str = "version";

pub const FORMATS_DOC: &str =
    "https://github.com/tylerdavis/graph/blob/main/docs/reference/file-versions.mdx";

pub type Migration = fn(&mut DocumentMut) -> Result<Vec<String>, String>;

const MIGRATIONS: &[Migration] = &[named_models_become_roles];

const RETIRED_ROLES: &[&str] = &["embedder", "use_case_solver"];

fn named_models_become_roles(doc: &mut DocumentMut) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let Some(models) = doc.get_mut("models") else {
        return Ok(notes);
    };
    match models {
        Item::Table(table) => hoist_named_into_table(table)?,
        Item::Value(Value::InlineTable(inline)) => hoist_named_into_inline(inline)?,
        _ => return Err("models must be a table".to_string()),
    }
    let Some(models) = models.as_table_like_mut() else {
        return Ok(notes);
    };
    for (name, entry) in models.iter_mut() {
        let dropped = match entry {
            Item::Table(table) => table.remove("dimensions").is_some(),
            Item::Value(Value::InlineTable(table)) => {
                let dropped = table.remove("dimensions").is_some();
                if dropped {
                    table.fmt();
                }
                dropped
            }
            _ => false,
        };
        if dropped {
            notes.push(format!(
                "dropped models.{}.dimensions: graph no longer reads it",
                name.get()
            ));
        }
        if RETIRED_ROLES.contains(&name.get()) {
            notes.push(format!(
                "models.{} is no longer a standard role: it stays as a custom role that nothing consults",
                name.get()
            ));
        }
    }
    Ok(notes)
}

fn named_collision(name: &str) -> String {
    format!(
        "models.named.{name} and models.{name} are both set; config version 2 has one models.{name}, so remove one of them before migrating"
    )
}

fn hoist_named_into_table(models: &mut toml_edit::Table) -> Result<(), String> {
    let Some((_, named)) = models.remove_entry("named") else {
        return Ok(());
    };
    let mut header_prefix = None;
    let mut position = None;
    let entries: Vec<(toml_edit::Key, Item)> = match named {
        Item::Table(mut table) => {
            if !table.is_implicit() {
                header_prefix = table.decor().prefix().cloned();
                position = table.position();
            }
            let keys: Vec<toml_edit::Key> = table
                .iter()
                .filter_map(|(name, _)| table.key(name).cloned())
                .collect();
            keys.into_iter()
                .filter_map(|key| table.remove(key.get()).map(|entry| (key, entry)))
                .collect()
        }
        Item::Value(Value::InlineTable(table)) => table
            .into_iter()
            .map(|(name, entry)| (toml_edit::Key::new(name), Item::Value(entry)))
            .collect(),
        _ => return Err("models.named must be a table of model entries".to_string()),
    };
    let mut header_prefix =
        header_prefix.filter(|prefix| !prefix.as_str().unwrap_or("").is_empty());
    if models.is_implicit() && position.is_some() {
        models.set_implicit(false);
        if let Some(position) = position {
            models.set_position(position);
        }
        if let Some(prefix) = header_prefix.take() {
            models.decor_mut().set_prefix(prefix);
        }
    }
    for (mut key, mut entry) in entries {
        if models.contains_key(key.get()) {
            return Err(named_collision(key.get()));
        }
        if let Some(prefix) = header_prefix.take() {
            let prefix = prefix.as_str().unwrap_or("");
            match &mut entry {
                Item::Table(table) if !table.is_implicit() => {
                    let own = table
                        .decor()
                        .prefix()
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let combined = format!("{prefix}{own}");
                    table.decor_mut().set_prefix(combined);
                }
                _ => {
                    let own = key
                        .leaf_decor()
                        .prefix()
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    let combined = format!("{prefix}{own}");
                    key.leaf_decor_mut().set_prefix(combined);
                }
            }
        }
        models.insert_formatted(&key, entry);
    }
    Ok(())
}

fn hoist_named_into_inline(models: &mut toml_edit::InlineTable) -> Result<(), String> {
    let Some(named) = models.remove("named") else {
        return Ok(());
    };
    let Value::InlineTable(named) = named else {
        return Err("models.named must be a table of model entries".to_string());
    };
    for (name, entry) in named {
        if models.contains_key(&name) {
            return Err(named_collision(&name));
        }
        models.insert(name, entry);
    }
    models.fmt();
    Ok(())
}

pub fn migration_count() -> usize {
    MIGRATIONS.len()
}

pub fn window_problem(
    kind: &str,
    what: &dyn fmt::Display,
    found: u32,
    oldest: u32,
    max: u32,
) -> Option<String> {
    if (oldest..=max).contains(&found) {
        return None;
    }
    let range = if oldest == max {
        format!("{kind} version {max}")
    } else {
        format!("{kind} versions {oldest} to {max}")
    };
    let version = env!("CARGO_PKG_VERSION");
    Some(if found > max {
        format!(
            "{what} is {kind} version {found}; graph {version} reads {range}. Upgrade graph, or see {FORMATS_DOC}"
        )
    } else {
        format!(
            "{what} is {kind} version {found}; graph {version} reads {range}. Migrate it with a graph release that still reads {kind} version {found}, or see {FORMATS_DOC}"
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub path: PathBuf,
    pub found: u32,
    pub oldest: u32,
    pub max: u32,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            &window_problem(
                "config",
                &self.path.display(),
                self.found,
                self.oldest,
                self.max,
            )
            .unwrap_or_default(),
        )
    }
}

impl std::error::Error for Unsupported {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    pub from: u32,
    pub to: u32,
    pub notes: Vec<String>,
}

impl Upgrade {
    pub fn migrated(&self) -> bool {
        self.from != self.to
    }
}

#[derive(Debug, Clone)]
pub struct LayerInfo {
    pub path: PathBuf,
    pub declared: Option<u32>,
    pub version: u32,
    pub notes: Vec<String>,
}

impl LayerInfo {
    pub fn needs_migration(&self) -> bool {
        self.version < CONFIG_FORMAT || self.declared.is_none()
    }
}

pub fn declared_version(doc: &DocumentMut) -> Result<Option<u32>, String> {
    match doc.get(FORMAT_KEY) {
        None => Ok(None),
        Some(Item::Value(Value::Integer(n))) => u32::try_from(*n.value())
            .ok()
            .filter(|n| *n >= 1)
            .map(Some)
            .ok_or_else(|| format!("{FORMAT_KEY} must be a positive integer, got {n}")),
        Some(other) => Err(format!(
            "{FORMAT_KEY} must be a positive integer, got {}",
            other.type_name()
        )),
    }
}

pub fn upgrade(doc: &mut DocumentMut, path: &Path) -> Result<Upgrade, String> {
    let from = declared_version(doc)?.unwrap_or(1);
    if !(CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT).contains(&from) {
        return Err(Unsupported {
            path: path.to_path_buf(),
            found: from,
            oldest: CONFIG_FORMAT_OLDEST,
            max: CONFIG_FORMAT,
        }
        .to_string());
    }
    let mut notes = Vec::new();
    for migration in &MIGRATIONS[(from - CONFIG_FORMAT_OLDEST) as usize..] {
        notes.extend(migration(doc)?);
    }
    Ok(Upgrade {
        from,
        to: CONFIG_FORMAT,
        notes,
    })
}

pub fn stamp(doc: &mut DocumentMut) {
    let root = doc.as_table_mut();
    let header = take_leading_prefix(root);
    root.remove(FORMAT_KEY);
    root.insert(FORMAT_KEY, toml_edit::value(i64::from(CONFIG_FORMAT)));
    if let (Some(prefix), Some(mut key)) = (header, root.key_mut(FORMAT_KEY)) {
        key.leaf_decor_mut().set_prefix(prefix);
    }
}

fn take_leading_prefix(root: &mut toml_edit::Table) -> Option<toml_edit::RawString> {
    let first = root
        .iter()
        .filter(|(_, item)| !item.is_none())
        .min_by_key(|(_, item)| match item {
            Item::Table(table) => table_position(table),
            Item::ArrayOfTables(_) => usize::MAX - 1,
            _ => 0,
        })
        .map(|(key, _)| key.to_string())?;
    match root.get_mut(&first)? {
        Item::Table(table) => {
            let table = first_header_table(table);
            let prefix = table.decor().prefix().cloned();
            table.decor_mut().set_prefix("\n");
            prefix
        }
        Item::Value(_) => {
            let mut key = root.key_mut(&first)?;
            let prefix = key.leaf_decor().prefix().cloned();
            key.leaf_decor_mut().set_prefix("");
            prefix
        }
        _ => None,
    }
}

fn table_position(table: &toml_edit::Table) -> usize {
    if !table.is_implicit() {
        return table.position().unwrap_or(usize::MAX);
    }
    table
        .iter()
        .filter_map(|(_, item)| item.as_table().map(table_position))
        .min()
        .unwrap_or(usize::MAX)
}

fn first_header_table(table: &mut toml_edit::Table) -> &mut toml_edit::Table {
    if !table.is_implicit() {
        return table;
    }
    let child = table
        .iter()
        .filter_map(|(key, item)| {
            item.as_table()
                .map(|t| (key.to_string(), table_position(t)))
        })
        .min_by_key(|(_, position)| *position)
        .map(|(key, _)| key);
    match child {
        Some(key) => first_header_table(table[&key].as_table_mut().expect("filtered to tables")),
        None => table,
    }
}

pub fn inspect(path: &Path) -> Result<LayerInfo, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let doc: DocumentMut = raw
        .parse()
        .map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let declared = declared_version(&doc)?;
    Ok(LayerInfo {
        path: path.to_path_buf(),
        declared,
        version: declared.unwrap_or(1),
        notes: Vec::new(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migrated {
    pub path: PathBuf,
    pub from: u32,
    pub to: u32,
    pub changed: bool,
    pub notes: Vec<String>,
}

pub fn migrate_file(path: &Path) -> Result<Migrated, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let upgrade = upgrade(&mut doc, path)?;
    stamp(&mut doc);
    let rendered = doc.to_string();
    let changed = rendered != raw;
    if changed {
        std::fs::write(path, rendered).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(Migrated {
        path: path.to_path_buf(),
        from: upgrade.from,
        to: upgrade.to,
        changed,
        notes: upgrade.notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_is_the_oldest_format() {
        let doc: DocumentMut = "[settings]\n".parse().unwrap();
        assert_eq!(declared_version(&doc).unwrap(), None);
    }

    #[test]
    fn non_integer_key_is_rejected() {
        let doc: DocumentMut = "version = \"2\"\n".parse().unwrap();
        let err = declared_version(&doc).unwrap_err();
        assert!(err.contains("positive integer"), "{err}");
        let doc: DocumentMut = "version = 0\n".parse().unwrap();
        assert!(declared_version(&doc).is_err());
    }

    #[test]
    fn newer_format_is_refused_by_name() {
        let mut doc: DocumentMut = format!("version = {}\n", CONFIG_FORMAT + 1)
            .parse()
            .unwrap();
        let err = upgrade(&mut doc, Path::new("/x/config.toml")).unwrap_err();
        assert!(err.contains("/x/config.toml is config version"), "{err}");
        let window = if CONFIG_FORMAT_OLDEST == CONFIG_FORMAT {
            format!("reads config version {CONFIG_FORMAT}")
        } else {
            format!("reads config versions {CONFIG_FORMAT_OLDEST} to {CONFIG_FORMAT}")
        };
        assert!(err.contains(&window), "{err}");
        assert!(err.contains("Upgrade graph"), "{err}");
        assert!(err.contains(FORMATS_DOC), "{err}");
    }

    #[test]
    fn the_window_message_names_both_directions() {
        assert_eq!(window_problem("plan", &"p.yaml", 3, 2, 4), None);
        let old = window_problem("plan", &"p.yaml", 1, 2, 4).unwrap();
        assert!(old.starts_with("p.yaml is plan version 1; graph "), "{old}");
        assert!(old.contains("reads plan versions 2 to 4"), "{old}");
        assert!(old.contains("still reads plan version 1"), "{old}");
        let new = window_problem("store", &"/data", 5, 2, 4).unwrap();
        assert!(
            new.contains("reads store versions 2 to 4. Upgrade graph"),
            "{new}"
        );
        let single = window_problem("tool", &"t.yaml", 2, 1, 1).unwrap();
        assert!(single.contains("reads tool version 1."), "{single}");
    }

    #[test]
    fn upgrade_reports_the_span() {
        let mut doc: DocumentMut = "version = 1\n[settings]\nhistory_limit = 3\n"
            .parse()
            .unwrap();
        let upgrade = upgrade(&mut doc, Path::new("c.toml")).unwrap();
        assert_eq!(upgrade.from, 1);
        assert_eq!(upgrade.to, CONFIG_FORMAT);
        assert_eq!(doc["settings"]["history_limit"].as_integer(), Some(3));
    }

    #[test]
    fn version_1_named_models_are_hoisted_with_their_comments() {
        let mut doc: DocumentMut = r#"[models]
default = { provider = "p", model = "m" }
embedder = { provider = "p", model = "e", dimensions = 768 }

# what the scout is for
[models.named.scout]
provider = "p"
model = "s"
description = "scouting"

[models.named.reviewer]
provider = "p"
model = "r"

[pricing."m"]
input = 1.0
output = 2.0
"#
        .parse()
        .unwrap();
        let upgrade = upgrade(&mut doc, Path::new("c.toml")).unwrap();
        assert_eq!(upgrade.from, 1);
        assert_eq!(
            upgrade.notes,
            vec![
                "dropped models.embedder.dimensions: graph no longer reads it",
                "models.embedder is no longer a standard role: it stays as a custom role that nothing consults"
            ]
        );
        assert_eq!(
            doc.to_string(),
            r#"[models]
default = { provider = "p", model = "m" }
embedder = { provider = "p", model = "e" }

# what the scout is for
[models.scout]
provider = "p"
model = "s"
description = "scouting"

[models.reviewer]
provider = "p"
model = "r"

[pricing."m"]
input = 1.0
output = 2.0
"#
        );
    }

    #[test]
    fn version_1_inline_named_models_are_hoisted() {
        let mut doc: DocumentMut =
            "[models]\nnamed = { nano = { provider = \"p\", model = \"n\" } }\n"
                .parse()
                .unwrap();
        upgrade(&mut doc, Path::new("c.toml")).unwrap();
        assert_eq!(doc["models"]["nano"]["model"].as_str(), Some("n"));
        assert!(doc["models"].get("named").is_none());
    }

    #[test]
    fn a_named_header_with_inline_entries_keeps_both_comments() {
        let mut doc: DocumentMut = r#"[models]
default = { provider = "p", model = "m" }

# Named models: one per review job
[models.named]
# cheap scout
scout = { provider = "p", model = "s" }
reviewer = { provider = "p", model = "r" }

[pricing."m"]
input = 1.0
"#
        .parse()
        .unwrap();
        upgrade(&mut doc, Path::new("c.toml")).unwrap();
        assert_eq!(
            doc.to_string(),
            r#"[models]
default = { provider = "p", model = "m" }

# Named models: one per review job
# cheap scout
scout = { provider = "p", model = "s" }
reviewer = { provider = "p", model = "r" }

[pricing."m"]
input = 1.0
"#
        );
    }

    #[test]
    fn a_named_header_under_an_implicit_models_table_keeps_its_place() {
        let mut doc: DocumentMut = r#"[providers.p]
type = "anthropic"

[models.named]
scout = { provider = "p", model = "s" }

[pricing."m"]
input = 1.0
"#
        .parse()
        .unwrap();
        upgrade(&mut doc, Path::new("c.toml")).unwrap();
        assert_eq!(
            doc.to_string(),
            r#"[providers.p]
type = "anthropic"

[models]
scout = { provider = "p", model = "s" }

[pricing."m"]
input = 1.0
"#
        );
    }

    #[test]
    fn retired_role_slots_are_noted_but_kept() {
        let mut doc: DocumentMut =
            "[models]\nuse_case_solver = { provider = \"p\", model = \"u\" }\n"
                .parse()
                .unwrap();
        let upgrade = upgrade(&mut doc, Path::new("c.toml")).unwrap();
        assert_eq!(
            upgrade.notes,
            vec!["models.use_case_solver is no longer a standard role: it stays as a custom role that nothing consults"]
        );
        assert_eq!(
            doc["models"]["use_case_solver"]["model"].as_str(),
            Some("u")
        );
    }

    #[test]
    fn a_named_model_colliding_with_a_role_refuses_to_migrate() {
        let mut doc: DocumentMut = "[models]\njudge = { provider = \"p\", model = \"m\" }\n[models.named.judge]\nprovider = \"p\"\nmodel = \"x\"\n"
            .parse()
            .unwrap();
        let err = upgrade(&mut doc, Path::new("c.toml")).unwrap_err();
        assert!(err.contains("models.named.judge and models.judge"), "{err}");
    }

    #[test]
    fn chain_length_matches_the_current_format() {
        assert_eq!(
            migration_count() as u32,
            CONFIG_FORMAT - CONFIG_FORMAT_OLDEST
        );
    }

    #[test]
    fn stamp_puts_the_key_first_and_keeps_comments() {
        let mut doc: DocumentMut = "# my config\n\n[settings]\n# why\nhistory_limit = 3\n"
            .parse()
            .unwrap();
        stamp(&mut doc);
        let out = doc.to_string();
        assert_eq!(
            out,
            format!("# my config\n\nversion = {CONFIG_FORMAT}\n\n[settings]\n# why\nhistory_limit = 3\n")
        );
        stamp(&mut doc);
        assert_eq!(doc.to_string(), out);
        let mut bare: DocumentMut = "[settings]\n".parse().unwrap();
        stamp(&mut bare);
        assert_eq!(
            bare.to_string(),
            format!("version = {CONFIG_FORMAT}\n\n[settings]\n")
        );
        let mut dotted: DocumentMut =
            "# project\n\n[providers.anthropic]\ntype = \"anthropic\"\n\n[models]\n"
                .parse()
                .unwrap();
        stamp(&mut dotted);
        assert_eq!(
            dotted.to_string(),
            format!(
                "# project\n\nversion = {CONFIG_FORMAT}\n\n[providers.anthropic]\ntype = \"anthropic\"\n\n[models]\n"
            )
        );
        let mut empty: DocumentMut = "".parse().unwrap();
        stamp(&mut empty);
        assert_eq!(empty.to_string(), format!("version = {CONFIG_FORMAT}\n"));
    }

    #[test]
    fn migrate_file_stamps_once_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[settings]\nhistory_limit = 3\n").unwrap();
        let first = migrate_file(&path).unwrap();
        assert!(first.changed);
        assert_eq!((first.from, first.to), (1, CONFIG_FORMAT));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("version = "), "{text}");
        let second = migrate_file(&path).unwrap();
        assert!(!second.changed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}
