//! Template parsing: `{{path}}`, `{{#section}}…{{/section}}`,
//! `{{^inverted}}…{{/inverted}}`, with dotted paths, numeric indices,
//! `.length`, and `@index`/`@first`/`@last` loop variables.

use super::RenderError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
    /// `.length` on arrays and strings.
    Length,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// Dotted data path, e.g. `E0.values.0.id`.
    Data(Vec<Seg>),
    /// Loop variables, only meaningful inside a section.
    AtIndex,
    AtFirst,
    AtLast,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Text(String),
    Var(Path),
    Section {
        path: Path,
        inverted: bool,
        body: Vec<Node>,
    },
}

pub fn parse(template: &str) -> Result<Vec<Node>, RenderError> {
    let mut nodes_stack: Vec<(Option<(Path, bool, String)>, Vec<Node>)> = vec![(None, Vec::new())];
    let mut rest = template;

    while let Some(open) = rest.find("{{") {
        if !rest[..open].is_empty() {
            top(&mut nodes_stack).push(Node::Text(rest[..open].to_string()));
        }
        let after = &rest[open + 2..];
        let close = after
            .find("}}")
            .ok_or_else(|| RenderError::Parse("unclosed {{ tag".to_string()))?;
        let raw_tag = after[..close].trim();
        rest = &after[close + 2..];

        match raw_tag.chars().next() {
            None => return Err(RenderError::Parse("empty {{}} tag".to_string())),
            Some('#') | Some('^') => {
                let inverted = raw_tag.starts_with('^');
                let name = raw_tag[1..].trim().to_string();
                let path = parse_path(&name)?;
                nodes_stack.push((Some((path, inverted, name)), Vec::new()));
            }
            Some('/') => {
                let name = raw_tag[1..].trim();
                let (header, body) = nodes_stack.pop().ok_or_else(|| {
                    RenderError::Parse(format!("unmatched closing tag {{{{/{name}}}}}"))
                })?;
                let Some((path, inverted, open_name)) = header else {
                    return Err(RenderError::Parse(format!(
                        "closing tag {{{{/{name}}}}} without an open section"
                    )));
                };
                if open_name != name {
                    return Err(RenderError::Parse(format!(
                        "section mismatch: opened {{{{#{open_name}}}}} but closed {{{{/{name}}}}}"
                    )));
                }
                top(&mut nodes_stack).push(Node::Section {
                    path,
                    inverted,
                    body,
                });
            }
            Some('>') | Some('<') | Some('$') | Some('!') => {
                return Err(RenderError::Parse(format!(
                    "unsupported tag {{{{{raw_tag}}}}} — partials, blocks, parents, and comments are not part of this dialect"
                )));
            }
            _ => {
                top(&mut nodes_stack).push(Node::Var(parse_path(raw_tag)?));
            }
        }
    }
    if !rest.is_empty() {
        top(&mut nodes_stack).push(Node::Text(rest.to_string()));
    }

    let (header, nodes) = nodes_stack.pop().expect("root frame");
    if let Some((_, _, name)) = header {
        return Err(RenderError::Parse(format!(
            "unclosed section {{{{#{name}}}}}"
        )));
    }
    if !nodes_stack.is_empty() {
        let unclosed: Vec<String> = nodes_stack
            .iter()
            .filter_map(|(h, _)| h.as_ref().map(|(_, _, n)| n.clone()))
            .collect();
        return Err(RenderError::Parse(format!(
            "unclosed sections: {}",
            unclosed.join(", ")
        )));
    }
    Ok(nodes)
}

fn top<'a>(stack: &'a mut [(Option<(Path, bool, String)>, Vec<Node>)]) -> &'a mut Vec<Node> {
    &mut stack.last_mut().expect("non-empty stack").1
}

pub fn parse_path(raw: &str) -> Result<Path, RenderError> {
    match raw {
        "@index" => return Ok(Path::AtIndex),
        "@first" => return Ok(Path::AtFirst),
        "@last" => return Ok(Path::AtLast),
        _ => {}
    }
    if raw.is_empty() {
        return Err(RenderError::Parse("empty path".to_string()));
    }
    let mut segs = Vec::new();
    for part in raw.split('.') {
        if part.is_empty() {
            return Err(RenderError::Parse(format!("empty segment in path '{raw}'")));
        }
        if part == "length" {
            segs.push(Seg::Length);
        } else if part.chars().all(|c| c.is_ascii_digit()) {
            segs.push(Seg::Index(part.parse().map_err(|_| {
                RenderError::Parse(format!("index too large in path '{raw}'"))
            })?));
        } else if part.starts_with('@') {
            return Err(RenderError::Parse(format!(
                "loop variable {part} cannot appear inside a path"
            )));
        } else {
            segs.push(Seg::Key(part.to_string()));
        }
    }
    Ok(Path::Data(segs))
}

/// If `template` consists of exactly one variable tag (allowing surrounding
/// whitespace inside the braces only), return its path — the typed-splice
/// case where the raw JSON value replaces the string wholesale.
pub fn exact_var(template: &str) -> Option<Path> {
    let inner = template.strip_prefix("{{")?.strip_suffix("}}")?;
    let inner = inner.trim();
    if inner.is_empty()
        || inner.contains("{{")
        || matches!(
            inner.chars().next(),
            Some('#' | '^' | '/' | '>' | '<' | '$' | '!')
        )
    {
        return None;
    }
    parse_path(inner).ok()
}
