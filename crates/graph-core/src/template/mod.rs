//! The `{{Ex.path}}` template dialect: a strict, typed, logic-less engine
//! for step inputs and solver templates.
//!
//! ## Contract
//! - Variables: `{{E0.values.0.id}}` — dotted keys, numeric indices,
//!   `.length` (arrays/strings) as the final segment.
//! - Roots: step ids (`E0`, `E1`, …) and `input`. Root-anchored paths are
//!   strict — failures carry a typed reason (see `RenderError`).
//! - Typed splice: a step-input string that is exactly one variable tag
//!   resolves to the raw JSON value (numbers stay numbers, arrays stay
//!   arrays). Mixed text interpolates; objects/arrays serialize as JSON.
//! - Sections: `{{#arr}}…{{/arr}}` iterates with a context stack;
//!   `{{@index}}`/`{{@first}}`/`{{@last}}` are available inside. `{{^x}}`
//!   renders when falsy/empty. Missing keys *under an existing root* are
//!   falsy in section position; bare keys missing on a list item render
//!   empty (items may omit optional fields).
//! - Not supported (dropped from the old dialect): partials, blocks,
//!   parents, comments, HTML escaping.

mod parser;
mod render;

pub use render::{render_input, render_str, render_value, Roots};

/// Parse a template and return the root names it references (`E0`, `input`,
/// …) — used by static plan validation to check reference ordering. Bare
/// keys inside a section body read the current item, not a root, so they
/// are not reported (same scoping rule as `rewrite_root` and
/// `referenced_paths`); dotted paths are root-anchored everywhere.
pub fn referenced_roots(template: &str) -> Result<Vec<String>, RenderError> {
    fn collect(nodes: &[parser::Node], in_section: bool, roots: &mut Vec<String>) {
        for node in nodes {
            match node {
                parser::Node::Var(parser::Path::Data(segs))
                | parser::Node::Section {
                    path: parser::Path::Data(segs),
                    ..
                } if !(in_section && segs.len() == 1) => {
                    if let Some(parser::Seg::Key(root)) = segs.first() {
                        if !roots.contains(root) {
                            roots.push(root.clone());
                        }
                    }
                }
                _ => {}
            }
            if let parser::Node::Section { body, .. } = node {
                collect(body, true, roots);
            }
        }
    }
    let nodes = parser::parse(template)?;
    let mut roots = Vec::new();
    collect(&nodes, false, &mut roots);
    Ok(roots)
}

/// Rewrite every reference whose root is `old` to `new` — `{{old.x}}` →
/// `{{new.x}}`, including section (`{{#old.x}}`), inverted (`{{^old.x}}`),
/// and closing (`{{/old.x}}`) tags. Used when a step id is renamed so
/// downstream templates keep working. A lexical pass that tracks section
/// depth: bare keys inside a section body are item-relative, never roots,
/// so they are left alone even when they equal `old`. Matching tags are
/// re-emitted with inner whitespace normalized (the parser trims anyway);
/// everything else — text, non-matching tags, malformed tags — passes
/// through byte-for-byte.
pub fn rewrite_root(template: &str, old: &str, new: &str) -> String {
    // Rewrite one tag, given whether bare keys are root-anchored here.
    fn rewrite_tag(raw: &str, old: &str, new: &str, at_root: bool) -> Option<String> {
        let trimmed = raw.trim();
        let (sigil, path) = match trimmed.chars().next() {
            Some(c @ ('#' | '^' | '/')) => (Some(c), trimmed[c.len_utf8()..].trim_start()),
            _ => (None, trimmed),
        };
        let (root, tail) = match path.split_once('.') {
            Some((root, tail)) => (root, Some(tail)),
            None => (path, None),
        };
        if root != old || (tail.is_none() && !at_root) {
            return None;
        }
        let mut tag = String::new();
        tag.extend(sigil);
        tag.push_str(new);
        if let Some(tail) = tail {
            tag.push('.');
            tag.push_str(tail);
        }
        Some(tag)
    }

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut depth: usize = 0;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break; // unclosed tag — leave the remainder untouched
        };
        let raw = &after[..close];
        // Closers pop before the rewrite decision so they resolve at the
        // same depth as their opener; openers push after.
        let sigil = raw.trim().chars().next();
        if sigil == Some('/') {
            depth = depth.saturating_sub(1);
        }
        out.push_str(&rest[..open]);
        match rewrite_tag(raw, old, new, depth == 0) {
            Some(tag) => {
                out.push_str("{{");
                out.push_str(&tag);
                out.push_str("}}");
            }
            None => out.push_str(&rest[open..open + 2 + close + 2]),
        }
        if matches!(sigil, Some('#') | Some('^')) {
            depth += 1;
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// Parse a template and return every full data path it references, dotted
/// (`E0.values.0.id`, `input.team`). Section paths are included; paths
/// referenced *inside* a section body relative to its items are emitted
/// with a `*` item marker (`{{#E1.values}}{{id}}{{/}}` → `E1.values.*.id`)
/// so callers can tell users which fields a value must contain. Loop
/// pseudo-vars (`@index` …) are omitted.
pub fn referenced_paths(template: &str) -> Result<Vec<String>, RenderError> {
    fn seg_text(seg: &parser::Seg) -> String {
        match seg {
            parser::Seg::Key(key) => key.clone(),
            parser::Seg::Index(index) => index.to_string(),
            parser::Seg::Length => "length".to_string(),
        }
    }

    fn path_text(segs: &[parser::Seg]) -> String {
        segs.iter().map(seg_text).collect::<Vec<_>>().join(".")
    }

    fn push(paths: &mut Vec<String>, path: String) {
        if !path.is_empty() && !path.starts_with('@') && !paths.contains(&path) {
            paths.push(path);
        }
    }

    fn collect(nodes: &[parser::Node], prefix: &str, paths: &mut Vec<String>) {
        for node in nodes {
            match node {
                parser::Node::Var(parser::Path::Data(segs)) => {
                    let text = path_text(segs);
                    // Multi-segment paths are root-anchored; bare keys
                    // inside a section read the current item.
                    if segs.len() > 1 || prefix.is_empty() {
                        push(paths, text);
                    } else {
                        push(paths, format!("{prefix}.*.{text}"));
                    }
                }
                parser::Node::Section {
                    path: parser::Path::Data(segs),
                    body,
                    ..
                } => {
                    let text = path_text(segs);
                    let anchored = segs.len() > 1 || prefix.is_empty();
                    let section_path = if anchored {
                        text.clone()
                    } else {
                        format!("{prefix}.*.{text}")
                    };
                    push(paths, section_path.clone());
                    collect(body, &section_path, paths);
                }
                parser::Node::Section { body, .. } => collect(body, prefix, paths),
                _ => {}
            }
        }
    }

    let nodes = parser::parse(template)?;
    let mut paths = Vec::new();
    collect(&nodes, "", &mut paths);
    Ok(paths)
}

/// Why a template failed to render. The caller decides policy:
/// `MissingStep`/`BadPath` are plan defects (replan in `plan_and_execute`,
/// hard failure for explicit plans); `EmptyData` means the plan was fine
/// but the data ran out (degrade to the solver, never replan).
#[derive(Debug, Clone, thiserror::Error)]
pub enum RenderError {
    #[error("template references step '{step}' which has no result (available: {})", available.join(", "))]
    MissingStep {
        step: String,
        available: Vec<String>,
    },
    #[error("bad path '{path}': {reason}")]
    BadPath { path: String, reason: String },
    #[error("empty data at '{path}': {reason}")]
    EmptyData { path: String, reason: String },
    #[error("template parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};

    fn roots(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    // ── Ported from the original renderInput.test.ts ────────────────────

    #[test]
    fn renders_a_simple_string() {
        let map = roots(&[("E0", json!({"name": "World"}))]);
        let out = render_str("Hello, {{E0.name}}!", &Roots::new(&map)).unwrap();
        assert_eq!(out, "Hello, World!");
    }

    #[test]
    fn handles_array_with_single_element() {
        let map = roots(&[("E0", json!({"teams": [{"name": "Red Team"}]}))]);
        let out = render_str("The team is {{E0.teams.0.name}}", &Roots::new(&map)).unwrap();
        assert_eq!(out, "The team is Red Team");
    }

    #[test]
    fn handles_multiple_replacements() {
        let map = roots(&[
            ("E0", json!({"teams": [{"name": "Red Team"}]})),
            ("E1", json!({"score": [{"value": 10}]})),
        ]);
        let out = render_str(
            "Team {{E0.teams.0.name}} scored {{E1.score.0.value}} points",
            &Roots::new(&map),
        )
        .unwrap();
        assert_eq!(out, "Team Red Team scored 10 points");
    }

    #[test]
    fn plain_field_access_works() {
        let map = roots(&[("E0", json!({"color": "Red"}))]);
        let out = render_str("The {{E0.color}} team", &Roots::new(&map)).unwrap();
        assert_eq!(out, "The Red team");
    }

    // ── New contract: typed splice ───────────────────────────────────────

    #[test]
    fn exact_tag_splices_raw_json_value() {
        let map = roots(&[("E0", json!({"count": 42, "ids": [1, 2, 3]}))]);
        let r = Roots::new(&map);
        assert_eq!(render_value("{{E0.count}}", &r).unwrap(), json!(42));
        assert_eq!(render_value("{{ E0.ids }}", &r).unwrap(), json!([1, 2, 3]));
        // Mixed text stays a string.
        assert_eq!(
            render_value("count: {{E0.count}}", &r).unwrap(),
            json!("count: 42")
        );
    }

    #[test]
    fn render_input_walks_the_json_tree() {
        let map = roots(&[
            ("E0", json!({"values": [{"id": "team-1"}]})),
            ("input", json!({"max": 50})),
        ]);
        let input = json!({
            "teamId": "{{E0.values.0.id}}",
            "limit": "{{input.max}}",
            "nested": {"q": "team {{E0.values.0.id}}"},
            "list": ["{{E0.values}}"],
        });
        let rendered = render_input(&input, &Roots::new(&map)).unwrap();
        assert_eq!(
            rendered,
            json!({
                "teamId": "team-1",
                "limit": 50,
                "nested": {"q": "team team-1"},
                "list": [[{"id": "team-1"}]],
            })
        );
    }

    // ── New contract: error taxonomy ─────────────────────────────────────

    #[test]
    fn missing_step_is_typed() {
        let map = roots(&[("E0", json!({}))]);
        let err = render_str("{{E7.values}}", &Roots::new(&map)).unwrap_err();
        assert!(matches!(err, RenderError::MissingStep { ref step, .. } if step == "E7"));
    }

    #[test]
    fn bad_path_reports_available_keys() {
        let map = roots(&[("E0", json!({"values": [{"id": "x", "name": "y"}]}))]);
        let err = render_str("{{E0.values.0.idd}}", &Roots::new(&map)).unwrap_err();
        let RenderError::BadPath { reason, .. } = &err else {
            panic!("expected BadPath, got {err:?}");
        };
        assert!(reason.contains("no key 'idd'"));
        assert!(reason.contains("id, name"));
    }

    #[test]
    fn empty_array_and_null_are_empty_data_not_bad_path() {
        let map = roots(&[("E0", json!({"values": [], "assignee": null}))]);
        let r = Roots::new(&map);
        let err = render_str("{{E0.values.0.id}}", &r).unwrap_err();
        assert!(
            matches!(err, RenderError::EmptyData { .. }),
            "empty array: {err:?}"
        );
        let err = render_str("{{E0.assignee.name}}", &r).unwrap_err();
        assert!(
            matches!(err, RenderError::EmptyData { .. }),
            "null walk: {err:?}"
        );
    }

    #[test]
    fn index_out_of_range_is_empty_data() {
        let map = roots(&[("E0", json!({"values": [{"id": 1}]}))]);
        let err = render_str("{{E0.values.3.id}}", &Roots::new(&map)).unwrap_err();
        assert!(matches!(err, RenderError::EmptyData { .. }));
    }

    // ── Sections, loop vars, length ──────────────────────────────────────

    #[test]
    fn sections_iterate_with_context_stack_and_loop_vars() {
        let map = roots(&[(
            "E1",
            json!({"values": [
                {"title": "Fix login", "state": "done"},
                {"title": "Add SSO", "state": "open"},
            ]}),
        )]);
        let template = "{{#E1.values}}{{title}} ({{state}}){{^@last}}, {{/@last}}{{/E1.values}}";
        let out = render_str(template, &Roots::new(&map)).unwrap();
        assert_eq!(out, "Fix login (done), Add SSO (open)");
    }

    #[test]
    fn at_index_and_at_first() {
        let map = roots(&[("E0", json!({"items": ["a", "b"]}))]);
        let template = "{{#E0.items}}{{@index}}:{{#@first}}first {{/@first}}{{/E0.items}}";
        let out = render_str(template, &Roots::new(&map)).unwrap();
        assert_eq!(out, "0:first 1:");
    }

    #[test]
    fn length_pseudo_key() {
        let map = roots(&[("E1", json!({"values": [1, 2, 3]}))]);
        let out = render_str("Total: {{E1.values.length}}", &Roots::new(&map)).unwrap();
        assert_eq!(out, "Total: 3");
    }

    #[test]
    fn inverted_section_on_empty_results() {
        let map = roots(&[("E0", json!({"values": []}))]);
        let template = "{{#E0.values}}{{id}}{{/E0.values}}{{^E0.values}}no results{{/E0.values}}";
        let out = render_str(template, &Roots::new(&map)).unwrap();
        assert_eq!(out, "no results");
    }

    #[test]
    fn missing_key_on_list_item_renders_empty() {
        let map = roots(&[(
            "E0",
            json!({"values": [{"name": "a", "desc": "has one"}, {"name": "b"}]}),
        )]);
        let template = "{{#E0.values}}{{name}}:{{desc}};{{/E0.values}}";
        let out = render_str(template, &Roots::new(&map)).unwrap();
        assert_eq!(out, "a:has one;b:;");
    }

    #[test]
    fn objects_interpolate_as_json_in_text() {
        let map = roots(&[("E0", json!({"values": [{"id": 1}]}))]);
        let out = render_str("data: {{E0.values}}", &Roots::new(&map)).unwrap();
        assert!(out.starts_with("data: ["));
        assert!(out.contains("\"id\": 1"));
    }

    #[test]
    fn nested_sections_reach_outer_and_root_scopes() {
        let map = roots(&[(
            "E0",
            json!({"teams": [{"name": "Core", "members": [{"who": "amy"}]}]}),
        )]);
        let template =
            "{{#E0.teams}}{{#members}}{{who}}@{{name}} of {{E0.teams.length}}{{/members}}{{/E0.teams}}";
        let out = render_str(template, &Roots::new(&map)).unwrap();
        assert_eq!(out, "amy@Core of 1");
    }

    // ── Dropped features fail loudly ─────────────────────────────────────

    #[test]
    fn dropped_mustache_features_are_parse_errors() {
        let map = roots(&[]);
        let r = Roots::new(&map);
        for template in [
            "{{> partial}}",
            "{{$block}}x{{/block}}",
            "{{!c}}",
            "{{<parent}}x{{/parent}}",
        ] {
            let err = render_str(template, &r).unwrap_err();
            assert!(
                matches!(err, RenderError::Parse(_)),
                "{template} should be rejected"
            );
        }
    }

    #[test]
    fn referenced_roots_skips_section_scoped_bare_keys() {
        // Bare keys inside a section body ({{severity}}, {{#url}}) read the
        // current item; dotted paths ({{E3.summary}}) stay root-anchored.
        let roots = referenced_roots(
            "{{#E5.comments}}{{severity}} {{E3.summary}} \
             {{#url}}[{{file}}]({{url}}){{/url}}{{/E5.comments}}\
             {{^E5.comments}}none for {{input.pr}}{{/E5.comments}}",
        )
        .unwrap();
        assert_eq!(roots, vec!["E5", "E3", "input"]);

        // At the top level a bare tag is a root reference.
        let roots = referenced_roots("{{E0}} {{#items}}{{name}}{{/items}}").unwrap();
        assert_eq!(roots, vec!["E0", "items"]);
    }

    #[test]
    fn referenced_paths_collects_full_dotted_paths() {
        let paths = referenced_paths("{{E0.values.0.id}} and {{input.team}}").unwrap();
        assert_eq!(paths, vec!["E0.values.0.id", "input.team"]);

        // Sections emit the section path plus item-relative paths with a
        // `*` marker; loop pseudo-vars are omitted.
        let paths = referenced_paths(
            "{{#E1.values}}{{id}}: {{state}} {{@index}}{{/E1.values}} total {{E1.values.length}}",
        )
        .unwrap();
        assert_eq!(
            paths,
            vec![
                "E1.values",
                "E1.values.*.id",
                "E1.values.*.state",
                "E1.values.length",
            ]
        );

        // Duplicates collapse; parse errors propagate.
        let paths = referenced_paths("{{E0.id}} {{E0.id}}").unwrap();
        assert_eq!(paths, vec!["E0.id"]);
        assert!(referenced_paths("{{> partial}}").is_err());
    }

    #[test]
    fn rewrite_root_renames_matching_references() {
        // Plain var, dotted path, exact tag.
        assert_eq!(rewrite_root("{{E0}}", "E0", "fetch"), "{{fetch}}");
        assert_eq!(
            rewrite_root("id: {{E0.values.0.id}}", "E0", "fetch"),
            "id: {{fetch.values.0.id}}"
        );
        // Section, inverted, and closing tags all carry the root.
        assert_eq!(
            rewrite_root(
                "{{#E0.values}}{{id}}{{/E0.values}}{{^E0.values}}none{{/E0.values}}",
                "E0",
                "fetch"
            ),
            "{{#fetch.values}}{{id}}{{/fetch.values}}{{^fetch.values}}none{{/fetch.values}}"
        );
        // Whitespace inside braces still matches (normalized on rewrite).
        assert_eq!(rewrite_root("{{ E0.id }}", "E0", "fetch"), "{{fetch.id}}");
    }

    #[test]
    fn rewrite_root_leaves_everything_else_alone() {
        // Other roots, including prefixes: E1 must not rewrite E10.
        assert_eq!(
            rewrite_root("{{E10.id}} {{input.x}}", "E1", "fetch"),
            "{{E10.id}} {{input.x}}"
        );
        // Non-matching tags keep their exact bytes (whitespace included).
        assert_eq!(rewrite_root("{{ E2.id }}", "E0", "fetch"), "{{ E2.id }}");
        // Bare keys inside a section body are item-relative, not roots —
        // even one that happens to equal the renamed id stays put.
        assert_eq!(
            rewrite_root("{{#E0.values}}{{title}}{{/E0.values}}", "title", "t"),
            "{{#E0.values}}{{title}}{{/E0.values}}"
        );
        // …but a dotted path anywhere is root-anchored and does rewrite,
        // and a bare tag at the top level is a root reference.
        assert_eq!(
            rewrite_root(
                "{{#items.rows}}{{E0.id}}{{/items.rows}} {{E0}}",
                "E0",
                "fetch"
            ),
            "{{#items.rows}}{{fetch.id}}{{/items.rows}} {{fetch}}"
        );
        // Plain text and malformed tags pass through untouched.
        assert_eq!(rewrite_root("no tags here", "E0", "fetch"), "no tags here");
        assert_eq!(rewrite_root("{{E0.id", "E0", "fetch"), "{{E0.id");
        assert_eq!(
            rewrite_root("{{E0.a}} then {{E0.b", "E0", "fetch"),
            "{{fetch.a}} then {{E0.b"
        );
    }

    #[test]
    fn mismatched_sections_are_parse_errors() {
        let map = roots(&[("E0", json!({"a": [1]}))]);
        let err = render_str("{{#E0.a}}x{{/E0.b}}", &Roots::new(&map)).unwrap_err();
        assert!(matches!(err, RenderError::Parse(_)));
    }
}
