use serde_yaml::{Mapping, Value};
use std::fmt;
use std::path::{Path, PathBuf};

pub use graph_config::FORMATS_DOC;

pub const PLAN_FORMAT: u32 = 1;

pub const PLAN_FORMAT_OLDEST: u32 = 1;

pub const TOOL_FORMAT: u32 = 1;

pub const TOOL_FORMAT_OLDEST: u32 = 1;

pub const FORMAT_KEY: &str = "version";

pub type Migration = fn(&mut Value) -> Result<Vec<String>, String>;

const PLAN_MIGRATIONS: &[Migration] = &[];

const TOOL_MIGRATIONS: &[Migration] = &[];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plan,
    Tool,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Plan => "plan",
            Kind::Tool => "tool",
        }
    }

    pub fn current(self) -> u32 {
        match self {
            Kind::Plan => PLAN_FORMAT,
            Kind::Tool => TOOL_FORMAT,
        }
    }

    pub fn oldest(self) -> u32 {
        match self {
            Kind::Plan => PLAN_FORMAT_OLDEST,
            Kind::Tool => TOOL_FORMAT_OLDEST,
        }
    }

    fn migrations(self) -> &'static [Migration] {
        match self {
            Kind::Plan => PLAN_MIGRATIONS,
            Kind::Tool => TOOL_MIGRATIONS,
        }
    }
}

pub fn graph_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    Unsupported {
        kind: Kind,
        found: u32,
        oldest: u32,
        max: u32,
    },
    Invalid(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Unsupported {
                kind,
                found,
                oldest,
                max,
            } => f.write_str(&window_message(*kind, "this file", *found, *oldest, *max)),
            FormatError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FormatError {}

pub fn window_message(kind: Kind, what: &str, found: u32, oldest: u32, max: u32) -> String {
    graph_config::window_problem(kind.label(), &what, found, oldest, max).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    pub declared: Option<u32>,
    pub from: u32,
    pub to: u32,
    pub notes: Vec<String>,
}

impl Upgrade {
    pub fn migrated(&self) -> bool {
        self.from != self.to
    }
}

pub fn declared_version(value: &Value) -> Result<Option<u32>, String> {
    let Some(mapping) = value.as_mapping() else {
        return Ok(None);
    };
    match mapping.get(FORMAT_KEY) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n >= 1)
            .map(Some)
            .ok_or_else(|| format!("{FORMAT_KEY} must be a positive integer, got {n}")),
        Some(other) => Err(format!(
            "{FORMAT_KEY} must be a positive integer, got {}",
            describe(other)
        )),
    }
}

fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

pub fn upgrade(kind: Kind, value: &mut Value) -> Result<Upgrade, FormatError> {
    let declared = declared_version(value).map_err(FormatError::Invalid)?;
    let from = declared.unwrap_or(1);
    let max = kind.current();
    if !(kind.oldest()..=max).contains(&from) {
        return Err(FormatError::Unsupported {
            kind,
            found: from,
            oldest: kind.oldest(),
            max,
        });
    }
    let mut notes = Vec::new();
    for migration in &kind.migrations()[(from - kind.oldest()) as usize..] {
        notes.extend(migration(value).map_err(FormatError::Invalid)?);
    }
    if let Some(mapping) = value.as_mapping_mut() {
        mapping.shift_remove(FORMAT_KEY);
    }
    Ok(Upgrade {
        declared,
        from,
        to: max,
        notes,
    })
}

pub fn stamp(kind: Kind, value: &mut Value) {
    let Some(mapping) = value.as_mapping_mut() else {
        return;
    };
    let mut rebuilt = Mapping::with_capacity(mapping.len() + 1);
    rebuilt.insert(
        Value::from(FORMAT_KEY),
        Value::from(u64::from(kind.current())),
    );
    for (key, item) in std::mem::take(mapping) {
        if key.as_str() != Some(FORMAT_KEY) {
            rebuilt.insert(key, item);
        }
    }
    *mapping = rebuilt;
}

pub fn for_each_step(doc: &mut Value, visit: &mut dyn FnMut(&mut Value)) {
    let Some(steps) = doc.get_mut("steps") else {
        return;
    };
    visit_body(steps, visit);
}

fn visit_body(body: &mut Value, visit: &mut dyn FnMut(&mut Value)) {
    match body {
        Value::Sequence(steps) => {
            for step in steps {
                visit_step(step, visit);
            }
        }
        Value::Mapping(_) => visit_step(body, visit),
        _ => {}
    }
}

fn visit_step(step: &mut Value, visit: &mut dyn FnMut(&mut Value)) {
    visit(step);
    let tool = step
        .get("tool_name")
        .or_else(|| step.get("toolName"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(input) = step.get_mut("input") else {
        return;
    };
    match tool.as_deref() {
        Some("decide") => {
            for side in ["then", "else"] {
                if let Some(branch) = input.get_mut(side) {
                    visit_body(branch, visit);
                }
            }
        }
        Some("map" | "reduce") => {
            if let Some(body) = input.get_mut("do") {
                visit_body(body, visit);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migrated {
    pub path: PathBuf,
    pub from: u32,
    pub to: u32,
    pub changed: bool,
    pub notes: Vec<String>,
}

pub fn migrate_file(
    kind: Kind,
    path: &Path,
    check: &dyn Fn(&Value) -> Result<(), String>,
) -> Result<Migrated, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut value: Value =
        serde_yaml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    if !value.is_mapping() {
        return Err(format!("{}: not a YAML mapping", path.display()));
    }
    let upgrade = upgrade(kind, &mut value).map_err(|e| match e {
        FormatError::Unsupported {
            kind,
            found,
            oldest,
            max,
        } => window_message(kind, &path.display().to_string(), found, oldest, max),
        other => format!("{}: {other}", path.display()),
    })?;
    stamp(kind, &mut value);
    check(&value).map_err(|e| format!("{}: after migration: {e}", path.display()))?;
    let (header, body_lines) = split_header(&raw);
    let body = serde_yaml::to_string(&value).map_err(|e| format!("{}: {e}", path.display()))?;
    let rendered = format!("{header}{body}");
    let changed = rendered != raw;
    let mut notes = upgrade.notes;
    let dropped = body_lines
        .lines()
        .filter(|line| line.trim_start().starts_with('#'))
        .count();
    if changed && dropped > 0 {
        notes.push(format!(
            "{dropped} comment line(s) below the leading comment block were not preserved"
        ));
    }
    if changed {
        std::fs::write(path, rendered).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(Migrated {
        path: path.to_path_buf(),
        from: upgrade.from,
        to: upgrade.to,
        changed,
        notes,
    })
}

fn split_header(raw: &str) -> (String, &str) {
    let mut consumed = 0;
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            consumed += line.len();
        } else {
            break;
        }
    }
    (raw[..consumed].to_string(), &raw[consumed..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Value {
        serde_yaml::from_str(text).unwrap()
    }

    #[test]
    fn chains_match_their_current_formats() {
        assert_eq!(
            PLAN_MIGRATIONS.len() as u32,
            PLAN_FORMAT - PLAN_FORMAT_OLDEST
        );
        assert_eq!(
            TOOL_MIGRATIONS.len() as u32,
            TOOL_FORMAT - TOOL_FORMAT_OLDEST
        );
    }

    #[test]
    fn missing_key_means_the_oldest_format_and_is_stripped_after_upgrade() {
        let mut value = yaml("identifier: p\nsteps: []\n");
        let first = upgrade(Kind::Plan, &mut value).unwrap();
        assert_eq!(first.declared, None);
        assert_eq!(first.from, PLAN_FORMAT_OLDEST);
        assert_eq!(first.to, PLAN_FORMAT);
        let mut value = yaml(&format!("version: {PLAN_FORMAT}\nidentifier: p\n"));
        let second = upgrade(Kind::Plan, &mut value).unwrap();
        assert_eq!(second.declared, Some(PLAN_FORMAT));
        assert!(value.get(FORMAT_KEY).is_none());
    }

    #[test]
    fn a_newer_format_is_refused_with_both_numbers() {
        let mut value = yaml(&format!("version: {}\nname: t\n", TOOL_FORMAT + 1));
        let err = upgrade(Kind::Tool, &mut value).unwrap_err();
        assert_eq!(
            err,
            FormatError::Unsupported {
                kind: Kind::Tool,
                found: TOOL_FORMAT + 1,
                oldest: TOOL_FORMAT_OLDEST,
                max: TOOL_FORMAT
            }
        );
        let text = err.to_string();
        assert!(text.contains("is tool version"), "{text}");
        assert!(
            text.contains(&format!("reads tool version {TOOL_FORMAT}")),
            "{text}"
        );
        assert!(text.contains(FORMATS_DOC), "{text}");
    }

    #[test]
    fn a_malformed_key_is_invalid() {
        let mut value = yaml("version: two\n");
        let err = upgrade(Kind::Plan, &mut value).unwrap_err();
        assert!(matches!(err, FormatError::Invalid(_)), "{err}");
        let mut value = yaml("version: 0\n");
        assert!(matches!(
            upgrade(Kind::Plan, &mut value),
            Err(FormatError::Invalid(_))
        ));
    }

    #[test]
    fn stamp_puts_the_key_first_and_is_idempotent() {
        let mut value = yaml("identifier: p\nname: P\n");
        stamp(Kind::Plan, &mut value);
        stamp(Kind::Plan, &mut value);
        let keys: Vec<&str> = value
            .as_mapping()
            .unwrap()
            .keys()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert_eq!(keys, vec![FORMAT_KEY, "identifier", "name"]);
        assert_eq!(value[FORMAT_KEY].as_u64(), Some(u64::from(PLAN_FORMAT)));
    }

    #[test]
    fn for_each_step_reaches_every_nested_body() {
        let mut value = yaml(
            r#"
steps:
  - id: E0
    tool_name: decide
    input:
      if: { value: "x", op: eq, to: "x" }
      then:
        - id: E1
          tool_name: map
          input:
            over: "{{input.items}}"
            do:
              tool_name: user__one
              input: {}
      else:
        tool_name: reduce
        input:
          over: "{{input.items}}"
          do:
            - id: E2
              tool_name: user__two
              input: {}
  - id: E3
    tool_name: user__three
    input:
      do: { not: a body }
"#,
        );
        let mut seen = Vec::new();
        for_each_step(&mut value, &mut |step| {
            seen.push(
                step.get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string(),
            );
        });
        assert_eq!(
            seen,
            vec![
                "decide",
                "map",
                "user__one",
                "reduce",
                "user__two",
                "user__three"
            ]
        );
    }

    #[test]
    fn migrate_file_keeps_the_header_and_notes_dropped_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.yaml");
        std::fs::write(
            &path,
            "# header line\n# second\n\nname: t\ndescription: d # trailing\nkind: reshape\nshape: {}\n",
        )
        .unwrap();
        let first = migrate_file(Kind::Tool, &path, &|_| Ok(())).unwrap();
        assert!(first.changed);
        assert_eq!((first.from, first.to), (TOOL_FORMAT_OLDEST, TOOL_FORMAT));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(&format!(
                "# header line\n# second\n\nversion: {TOOL_FORMAT}\nname: t\n"
            )),
            "{text}"
        );
        assert!(first.notes.is_empty(), "{:?}", first.notes);
        let second = migrate_file(Kind::Tool, &path, &|_| Ok(())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!second.changed);
    }

    #[test]
    fn migrate_file_reports_comments_it_could_not_keep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.yaml");
        std::fs::write(&path, "identifier: p\n# inside\nname: P\n").unwrap();
        let migrated = migrate_file(Kind::Plan, &path, &|_| Ok(())).unwrap();
        assert!(migrated.changed);
        assert_eq!(migrated.notes.len(), 1, "{:?}", migrated.notes);
        assert!(migrated.notes[0].starts_with("1 comment line"));
    }

    #[test]
    fn migrate_file_refuses_a_result_the_check_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.yaml");
        let original = "identifier: p\n";
        std::fs::write(&path, original).unwrap();
        let err = migrate_file(Kind::Plan, &path, &|_| Err("no steps".into())).unwrap_err();
        assert!(err.contains("after migration: no steps"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
