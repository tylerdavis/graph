//! Evaluation: strict root resolution with a typed error taxonomy, context
//! stack for sections, loop variables, and typed splice.

use super::parser::{parse, Node, Path, Seg};
use super::RenderError;
use serde_json::{Map, Value};

/// Root values available to templates: step results under `E0`, `E1`, …
/// and plan inputs under `input`.
pub struct Roots<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Roots<'a> {
    pub fn new(map: &'a Map<String, Value>) -> Self {
        Self { map }
    }
}

/// Render a template string against the roots.
pub fn render_str(template: &str, roots: &Roots) -> Result<String, RenderError> {
    let nodes = parse(template)?;
    let mut renderer = Renderer {
        roots,
        contexts: Vec::new(),
        loops: Vec::new(),
    };
    let mut out = String::new();
    renderer.render_nodes(&nodes, &mut out)?;
    Ok(out)
}

/// Render one string in a step-input position: a string that is exactly one
/// variable tag splices the raw JSON value (typed); anything else renders
/// to a string.
pub fn render_value(template: &str, roots: &Roots) -> Result<Value, RenderError> {
    if let Some(Path::Data(segs)) = super::parser::exact_var(template) {
        return resolve_root(roots, &segs);
    }
    Ok(Value::String(render_str(template, roots)?))
}

/// Recursively render every string in a JSON tree (step inputs).
pub fn render_input(input: &Value, roots: &Roots) -> Result<Value, RenderError> {
    Ok(match input {
        Value::String(s) => render_value(s, roots)?,
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_input(item, roots))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), render_input(v, roots)?)))
                .collect::<Result<Map<_, _>, RenderError>>()?,
        ),
        other => other.clone(),
    })
}

/// Walk a root-anchored path strictly, classifying failures.
fn resolve_root(roots: &Roots, segs: &[Seg]) -> Result<Value, RenderError> {
    let Some(Seg::Key(root)) = segs.first() else {
        return Err(RenderError::Parse(
            "path must start with a root name".to_string(),
        ));
    };
    let Some(start) = roots.map.get(root.as_str()) else {
        return Err(RenderError::MissingStep {
            step: root.clone(),
            available: roots.map.keys().cloned().collect(),
        });
    };
    walk(start, &segs[1..], root)
}

fn walk(start: &Value, segs: &[Seg], root: &str) -> Result<Value, RenderError> {
    let mut current = start;
    let mut traversed = vec![root.to_string()];
    for (i, seg) in segs.iter().enumerate() {
        let at = traversed.join(".");
        match seg {
            Seg::Length => {
                let len = match current {
                    Value::Array(items) => items.len(),
                    Value::String(s) => s.chars().count(),
                    other => {
                        return Err(RenderError::BadPath {
                            path: format!("{at}.length"),
                            reason: format!(
                                ".length applies to arrays and strings, found {}",
                                kind(other)
                            ),
                        })
                    }
                };
                if i != segs.len() - 1 {
                    return Err(RenderError::BadPath {
                        path: at,
                        reason: ".length must be the final segment".to_string(),
                    });
                }
                return Ok(Value::Number(len.into()));
            }
            Seg::Index(idx) => match current {
                Value::Array(items) => match items.get(*idx) {
                    Some(item) => {
                        current = item;
                        traversed.push(idx.to_string());
                    }
                    None => {
                        return Err(RenderError::EmptyData {
                            path: full_path(&traversed, &segs[i..]),
                            reason: if items.is_empty() {
                                format!("{at} is an empty array")
                            } else {
                                format!(
                                    "{at} has {} items; index {idx} is out of range",
                                    items.len()
                                )
                            },
                        })
                    }
                },
                Value::Null => {
                    return Err(RenderError::EmptyData {
                        path: full_path(&traversed, &segs[i..]),
                        reason: format!("{at} is null"),
                    })
                }
                other => {
                    return Err(RenderError::BadPath {
                        path: full_path(&traversed, &segs[i..]),
                        reason: format!("cannot index into {} at {at}", kind(other)),
                    })
                }
            },
            Seg::Key(key) => match current {
                Value::Object(map) => match map.get(key) {
                    Some(child) => {
                        current = child;
                        traversed.push(key.clone());
                    }
                    None => {
                        let mut available: Vec<&str> = map.keys().map(String::as_str).collect();
                        available.truncate(12);
                        return Err(RenderError::BadPath {
                            path: full_path(&traversed, &segs[i..]),
                            reason: format!(
                                "no key '{key}' at {at} (available: {})",
                                available.join(", ")
                            ),
                        });
                    }
                },
                Value::Null => {
                    return Err(RenderError::EmptyData {
                        path: full_path(&traversed, &segs[i..]),
                        reason: format!("{at} is null"),
                    })
                }
                other => {
                    return Err(RenderError::BadPath {
                        path: full_path(&traversed, &segs[i..]),
                        reason: format!("cannot read key '{key}' from {} at {at}", kind(other)),
                    })
                }
            },
        }
    }
    Ok(current.clone())
}

fn full_path(traversed: &[String], remaining: &[Seg]) -> String {
    let mut parts = traversed.to_vec();
    for seg in remaining {
        parts.push(match seg {
            Seg::Key(k) => k.clone(),
            Seg::Index(i) => i.to_string(),
            Seg::Length => "length".to_string(),
        });
    }
    parts.join(".")
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

struct LoopMeta {
    index: usize,
    len: usize,
}

struct Renderer<'a> {
    roots: &'a Roots<'a>,
    /// Innermost-last stack of section contexts (owned clones — section
    /// bodies are small and this keeps lifetimes trivial).
    contexts: Vec<Value>,
    loops: Vec<LoopMeta>,
}

impl Renderer<'_> {
    fn render_nodes(&mut self, nodes: &[Node], out: &mut String) -> Result<(), RenderError> {
        for node in nodes {
            match node {
                Node::Text(text) => out.push_str(text),
                Node::Var(path) => {
                    if let Some(value) = self.lookup_var(path)? {
                        out.push_str(&to_text(&value));
                    }
                }
                Node::Section {
                    path,
                    inverted,
                    body,
                } => self.render_section(path, *inverted, body, out)?,
            }
        }
        Ok(())
    }

    /// Resolve a variable tag. Root-anchored paths are strict; bare keys
    /// inside sections search the context stack and render empty when the
    /// key is genuinely absent (list items may omit optional fields).
    fn lookup_var(&self, path: &Path) -> Result<Option<Value>, RenderError> {
        match path {
            Path::AtIndex => Ok(Some(Value::Number(
                self.current_loop("@index")?.index.into(),
            ))),
            Path::AtFirst => Ok(Some(Value::Bool(self.current_loop("@first")?.index == 0))),
            Path::AtLast => {
                let meta = self.current_loop("@last")?;
                Ok(Some(Value::Bool(meta.index + 1 == meta.len)))
            }
            Path::Data(segs) => {
                if self.is_root_anchored(segs) {
                    return resolve_root(self.roots, segs).map(Some);
                }
                for context in self.contexts.iter().rev() {
                    if let Ok(value) = walk(context, segs, "<item>") {
                        return Ok(Some(value));
                    }
                }
                if self.contexts.is_empty() {
                    // Outside a section, a bare path is a root reference with
                    // an unknown root — surface the strict error.
                    resolve_root(self.roots, segs).map(Some)
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Sections are conditionals: only a missing root errors; a missing key
    /// under an existing root resolves to falsy (mustache convention, so
    /// `{{^E0.optional}}fallback{{/E0.optional}}` works).
    fn lookup_section(&self, path: &Path) -> Result<Option<Value>, RenderError> {
        match path {
            Path::Data(segs) if self.is_root_anchored(segs) => {
                match resolve_root(self.roots, segs) {
                    Ok(value) => Ok(Some(value)),
                    Err(e @ RenderError::MissingStep { .. }) => Err(e),
                    Err(_) => Ok(None),
                }
            }
            _ => self.lookup_var(path),
        }
    }

    fn render_section(
        &mut self,
        path: &Path,
        inverted: bool,
        body: &[Node],
        out: &mut String,
    ) -> Result<(), RenderError> {
        let value = self.lookup_section(path)?;
        let truthy = match &value {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(Value::Array(items)) => !items.is_empty(),
            Some(Value::String(s)) => !s.is_empty(),
            Some(_) => true,
        };

        if inverted {
            if !truthy {
                self.render_nodes(body, out)?;
            }
            return Ok(());
        }
        if !truthy {
            return Ok(());
        }

        match value.expect("truthy value present") {
            Value::Array(items) => {
                let len = items.len();
                for (index, item) in items.into_iter().enumerate() {
                    self.loops.push(LoopMeta { index, len });
                    self.contexts.push(item);
                    let result = self.render_nodes(body, out);
                    self.contexts.pop();
                    self.loops.pop();
                    result?;
                }
                Ok(())
            }
            object @ Value::Object(_) => {
                self.contexts.push(object);
                let result = self.render_nodes(body, out);
                self.contexts.pop();
                result
            }
            _ => self.render_nodes(body, out),
        }
    }

    fn is_root_anchored(&self, segs: &[Seg]) -> bool {
        matches!(segs.first(), Some(Seg::Key(root)) if self.roots.map.contains_key(root.as_str()))
    }

    fn current_loop(&self, name: &str) -> Result<&LoopMeta, RenderError> {
        self.loops.last().ok_or_else(|| {
            RenderError::Parse(format!("{name} used outside of a section over an array"))
        })
    }
}

/// Scalar interpolation: objects and arrays serialize to JSON, scalars
/// print bare, null prints nothing.
fn to_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}
