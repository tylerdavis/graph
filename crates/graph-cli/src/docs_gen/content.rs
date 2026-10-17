use serde_yaml::Value as Yaml;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub struct Content {
    pub summary: String,
    pub body: String,
    pub fields: BTreeMap<String, FieldDoc>,
    pub output: Option<Yaml>,
}

pub struct FieldDoc {
    pub kind: Option<String>,
    pub description: String,
}

thread_local! {
    static USED: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) };
    static MISSING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/docs_gen/content")
}

fn seeding() -> bool {
    std::env::var(super::UPDATE_ENV).is_ok_and(|value| value == "1")
}

pub fn content(key: &str, fallback: &str) -> Content {
    USED.with(|used| used.borrow_mut().insert(key.to_string()));
    let path = dir().join(format!("{key}.md"));
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => {
            if seeding() {
                let seed = seed(fallback);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &seed).unwrap();
                seed
            } else {
                MISSING.with(|missing| missing.borrow_mut().push(key.to_string()));
                seed(fallback)
            }
        }
    };
    parse(&raw, &path)
}

fn seed(fallback: &str) -> String {
    let flat = fallback.split_whitespace().collect::<Vec<_>>().join(" ");
    let summary = super::mdx::first_sentence(&flat);
    let rest = flat[summary.len()..].trim();
    let mut out = format!(
        "---\nsummary: {}\n---\n",
        serde_yaml::to_string(&summary).unwrap().trim()
    );
    if !rest.is_empty() {
        out.push('\n');
        out.push_str(rest);
        out.push('\n');
    }
    out
}

fn parse(raw: &str, path: &Path) -> Content {
    let (front, body) = raw
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(front, body)| (front, body.trim_start_matches('\n')))
        .unwrap_or(("", raw));
    let meta: Yaml = serde_yaml::from_str(front)
        .unwrap_or_else(|e| panic!("{}: front matter is not YAML: {e}", path.display()));
    let summary = meta
        .get("summary")
        .and_then(Yaml::as_str)
        .unwrap_or_default()
        .to_string();
    let mut fields = BTreeMap::new();
    if let Some(map) = meta.get("fields").and_then(Yaml::as_mapping) {
        for (name, value) in map {
            let Some(name) = name.as_str() else { continue };
            let doc = match value {
                Yaml::String(description) => FieldDoc {
                    kind: None,
                    description: description.clone(),
                },
                other => FieldDoc {
                    kind: other.get("type").and_then(Yaml::as_str).map(str::to_string),
                    description: other
                        .get("description")
                        .and_then(Yaml::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
            };
            fields.insert(name.to_string(), doc);
        }
    }
    Content {
        summary,
        body: mdx_body(body.trim()),
        fields,
        output: meta.get("output").cloned(),
    }
}

fn mdx_body(body: &str) -> String {
    let mut fenced = false;
    body.lines()
        .map(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return line.to_string();
            }
            if fenced {
                line.to_string()
            } else {
                super::mdx::text_line(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn apply_fields(schema: &mut Yaml, fields: &BTreeMap<String, FieldDoc>) {
    for (path, doc) in fields {
        let segments: Vec<&str> = path.split('.').collect();
        let Some(node) = descend(schema, &segments) else {
            continue;
        };
        if let Some(map) = node.as_mapping_mut() {
            map.insert("description".into(), doc.description.clone().into());
            if let Some(kind) = &doc.kind {
                map.insert("type".into(), kind.clone().into());
            }
        }
    }
}

fn descend<'a>(node: &'a mut Yaml, segments: &[&str]) -> Option<&'a mut Yaml> {
    let Some((first, rest)) = segments.split_first() else {
        return Some(node);
    };
    let next = match *first {
        "[]" => node.get_mut("items")?,
        "<name>" => node.get_mut("additionalProperties")?,
        name => node.get_mut("properties")?.get_mut(name)?,
    };
    descend(next, rest)
}

pub fn problems() -> Vec<String> {
    let mut problems: Vec<String> = MISSING
        .with(|missing| missing.borrow().clone())
        .into_iter()
        .map(|key| format!("no docs description at src/docs_gen/content/{key}.md"))
        .collect();
    let used = USED.with(|used| used.borrow().clone());
    let mut existing = Vec::new();
    collect(&dir(), &dir(), &mut existing);
    for key in existing {
        if !used.contains(&key) {
            if seeding() {
                let _ = std::fs::remove_file(dir().join(format!("{key}.md")));
            } else {
                problems.push(format!(
                    "src/docs_gen/content/{key}.md describes something that no longer exists"
                ));
            }
        }
    }
    problems
}

fn collect(dir: &Path, root: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, root, found);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            let relative = path.strip_prefix(root).unwrap().with_extension("");
            found.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}
