use std::fmt;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Value};

pub const CONFIG_FORMAT: u32 = 1;

pub const CONFIG_FORMAT_OLDEST: u32 = 1;

pub const FORMAT_KEY: &str = "format_version";

pub const FORMATS_DOC: &str =
    "https://github.com/tylerdavis/graph/blob/main/docs/reference/formats.mdx";

pub type Migration = fn(&mut DocumentMut) -> Result<Vec<String>, String>;

const MIGRATIONS: &[Migration] = &[];

pub fn migration_count() -> usize {
    MIGRATIONS.len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooNew {
    pub path: PathBuf,
    pub found: u32,
    pub max: u32,
}

impl fmt::Display for TooNew {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is config format {}; graph {} reads up to {}. Upgrade graph, or see {FORMATS_DOC}",
            self.path.display(),
            self.found,
            env!("CARGO_PKG_VERSION"),
            self.max
        )
    }
}

impl std::error::Error for TooNew {}

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
    pub format_version: u32,
    pub notes: Vec<String>,
}

impl LayerInfo {
    pub fn needs_migration(&self) -> bool {
        self.format_version < CONFIG_FORMAT || self.declared.is_none()
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
    let from = declared_version(doc)?.unwrap_or(CONFIG_FORMAT_OLDEST);
    if from > CONFIG_FORMAT {
        return Err(TooNew {
            path: path.to_path_buf(),
            found: from,
            max: CONFIG_FORMAT,
        }
        .to_string());
    }
    let mut notes = Vec::new();
    for migration in &MIGRATIONS[(from - 1) as usize..] {
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
        format_version: declared.unwrap_or(CONFIG_FORMAT_OLDEST),
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
        let doc: DocumentMut = "format_version = \"2\"\n".parse().unwrap();
        let err = declared_version(&doc).unwrap_err();
        assert!(err.contains("positive integer"), "{err}");
        let doc: DocumentMut = "format_version = 0\n".parse().unwrap();
        assert!(declared_version(&doc).is_err());
    }

    #[test]
    fn newer_format_is_refused_by_name() {
        let mut doc: DocumentMut = format!("format_version = {}\n", CONFIG_FORMAT + 1)
            .parse()
            .unwrap();
        let err = upgrade(&mut doc, Path::new("/x/config.toml")).unwrap_err();
        assert!(err.contains("/x/config.toml is config format"), "{err}");
        assert!(
            err.contains(&format!("reads up to {CONFIG_FORMAT}")),
            "{err}"
        );
        assert!(err.contains(FORMATS_DOC), "{err}");
    }

    #[test]
    fn upgrade_reports_the_span() {
        let mut doc: DocumentMut = "format_version = 1\n[settings]\nhistory_limit = 3\n"
            .parse()
            .unwrap();
        let upgrade = upgrade(&mut doc, Path::new("c.toml")).unwrap();
        assert_eq!(upgrade.from, 1);
        assert_eq!(upgrade.to, CONFIG_FORMAT);
        assert_eq!(doc["settings"]["history_limit"].as_integer(), Some(3));
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
            format!("# my config\n\nformat_version = {CONFIG_FORMAT}\n\n[settings]\n# why\nhistory_limit = 3\n")
        );
        stamp(&mut doc);
        assert_eq!(doc.to_string(), out);
        let mut bare: DocumentMut = "[settings]\n".parse().unwrap();
        stamp(&mut bare);
        assert_eq!(
            bare.to_string(),
            format!("format_version = {CONFIG_FORMAT}\n\n[settings]\n")
        );
        let mut dotted: DocumentMut =
            "# project\n\n[providers.anthropic]\ntype = \"anthropic\"\n\n[models]\n"
                .parse()
                .unwrap();
        stamp(&mut dotted);
        assert_eq!(
            dotted.to_string(),
            format!(
                "# project\n\nformat_version = {CONFIG_FORMAT}\n\n[providers.anthropic]\ntype = \"anthropic\"\n\n[models]\n"
            )
        );
        let mut empty: DocumentMut = "".parse().unwrap();
        stamp(&mut empty);
        assert_eq!(
            empty.to_string(),
            format!("format_version = {CONFIG_FORMAT}\n")
        );
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
        assert!(text.starts_with("format_version = "), "{text}");
        let second = migrate_file(&path).unwrap();
        assert!(!second.changed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}
