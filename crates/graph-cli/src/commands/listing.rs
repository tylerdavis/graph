//! Shared renderer for grouped tool listings — `graph tools list` and
//! `graph mcp tools`.

use graph_core::ToolDef;
use graph_mcp::NAMESPACE_SEPARATOR;
use serde_json::{json, Value};

/// The source a namespaced tool belongs to (`linear__list_issues` →
/// `linear`). Bare names like `plan_and_execute` are core.
fn source_of(name: &str) -> &str {
    name.split_once(NAMESPACE_SEPARATOR)
        .map_or("(core)", |(source, _)| source)
}

/// One tool as JSON for `graph tools show --json`: the whole definition the
/// agent and planner see, losslessly — including the `inputSchema` a caller
/// needs to write a plan step's input object.
///
/// `outputSchema` is what the tool *declares*; `graph shapes show` is what it
/// has actually been observed to return.
pub fn tool_as_json(def: &ToolDef) -> Value {
    json!({
        "name": def.name,
        "source": source_of(&def.name),
        "description": def.description,
        "readOnly": def.read_only,
        "inputSchema": def.input_schema,
        "outputSchema": def.output_schema,
        "outputExample": def.output_example,
    })
}

/// Machine-readable twin of [`render_tool_listing`] for `--json`: the flat
/// catalog in discovery order, each entry carrying the namespaced `name` an
/// input actually has to use, plus a `sources` roll-up mirroring the text
/// renderer's sections.
///
/// Schemas are deliberately absent — a catalog of a few hundred tools would
/// bury the names, which is what a caller enumerates for. `graph tools show`
/// is the per-tool schema surface.
pub fn tool_listing_as_json(defs: &[ToolDef]) -> Value {
    let mut sources: Vec<(&str, usize)> = Vec::new();
    for def in defs {
        let source = source_of(&def.name);
        match sources.iter_mut().find(|(name, _)| *name == source) {
            Some((_, count)) => *count += 1,
            None => sources.push((source, 1)),
        }
    }

    json!({
        "tools": defs.iter().map(|def| json!({
            "name": def.name,
            "source": source_of(&def.name),
            "description": def.description,
            "readOnly": def.read_only,
            "hasOutputSchema": def.output_schema.is_some(),
        })).collect::<Vec<_>>(),
        "count": defs.len(),
        "sources": sources.iter().map(|(source, count)| json!({
            "source": source,
            "count": count,
        })).collect::<Vec<_>>(),
    })
}

/// Group namespaced defs by source prefix, one section per source: an
/// emphasized header with the tool count, then one entry per tool — bold
/// indented name over its one-line description. Blank lines separate
/// sections, not entries. Bare names (`plan_and_execute`) group under
/// "(core)".
pub fn render_tool_listing(defs: &[ToolDef], color: bool) -> String {
    let header = |s: &str| {
        if color {
            format!("\x1b[1;4m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let bold = |s: &str| {
        if color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let dim = |s: &str| {
        if color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };

    // Group by namespace prefix, preserving discovery order.
    let mut groups: Vec<(&str, Vec<&ToolDef>)> = Vec::new();
    for def in defs {
        let source = source_of(&def.name);
        match groups.iter_mut().find(|(name, _)| *name == source) {
            Some((_, tools)) => tools.push(def),
            None => groups.push((source, vec![def])),
        }
    }

    let mut out = String::new();
    for (i, (source, tools)) in groups.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let noun = if tools.len() == 1 { "tool" } else { "tools" };
        out.push_str(&format!(
            "{} {}\n",
            header(source),
            dim(&format!("— {} {noun}", tools.len()))
        ));
        for def in tools {
            let bare = def
                .name
                .split_once(NAMESPACE_SEPARATOR)
                .map_or(def.name.as_str(), |(_, bare)| bare);
            let marker = match def.read_only {
                Some(true) => format!(" {}", dim("[read-only]")),
                _ => String::new(),
            };
            out.push_str(&format!("  {}{marker}\n", bold(bare)));
            let description = def.description.lines().next().unwrap_or_default().trim();
            if !description.is_empty() {
                out.push_str(&format!("  {}\n", dim(description)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, description: &str, read_only: Option<bool>) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            output_example: None,
            read_only,
        }
    }

    #[test]
    fn groups_by_source_with_packed_entries() {
        let defs = vec![
            def("everything__echo", "Echoes back the input.", None),
            def(
                "everything__get-sum",
                "Adds two numbers.\nSecond line.",
                Some(true),
            ),
            def("linear__list_issues", "List issues.", Some(true)),
        ];
        let rendered = render_tool_listing(&defs, false);
        assert_eq!(
            rendered,
            "everything — 2 tools\n\
             \x20 echo\n\
             \x20 Echoes back the input.\n\
             \x20 get-sum [read-only]\n\
             \x20 Adds two numbers.\n\
             \n\
             linear — 1 tool\n\
             \x20 list_issues [read-only]\n\
             \x20 List issues.\n"
        );
    }

    #[test]
    fn bare_names_group_under_core() {
        let defs = vec![
            def("plan__project_status", "Project status report.", None),
            def("plan_and_execute", "Plan and execute a task.", None),
        ];
        let rendered = render_tool_listing(&defs, false);
        assert_eq!(
            rendered,
            "plan — 1 tool\n\
             \x20 project_status\n\
             \x20 Project status report.\n\
             \n\
             (core) — 1 tool\n\
             \x20 plan_and_execute\n\
             \x20 Plan and execute a task.\n"
        );
    }

    #[test]
    fn empty_description_omits_the_line() {
        let defs = vec![def("s__bare", "", None), def("s__t", "Desc.", None)];
        let rendered = render_tool_listing(&defs, false);
        assert_eq!(rendered, "s — 2 tools\n  bare\n  t\n  Desc.\n");
    }

    #[test]
    fn show_json_carries_the_whole_definition() {
        let mut with_schema = def("linear__list_issues", "List issues.", Some(true));
        with_schema.input_schema =
            json!({"type": "object", "properties": {"team": {"type": "string"}}});
        with_schema.output_schema = Some(json!({"type": "object"}));
        with_schema.output_example = Some(json!({"issues": []}));
        assert_eq!(
            tool_as_json(&with_schema),
            json!({
                "name": "linear__list_issues",
                "source": "linear",
                "description": "List issues.",
                "readOnly": true,
                "inputSchema": {"type": "object", "properties": {"team": {"type": "string"}}},
                "outputSchema": {"type": "object"},
                "outputExample": {"issues": []},
            })
        );
    }

    #[test]
    fn show_json_keeps_absent_optionals_as_null() {
        // Every key is always present, so a caller can address
        // `.outputSchema` without checking whether it exists first.
        assert_eq!(
            tool_as_json(&def("plan_and_execute", "Plan and execute.", None)),
            json!({
                "name": "plan_and_execute",
                "source": "(core)",
                "description": "Plan and execute.",
                "readOnly": null,
                "inputSchema": {},
                "outputSchema": null,
                "outputExample": null,
            })
        );
    }

    #[test]
    fn json_carries_namespaced_names_and_a_source_rollup() {
        let defs = vec![
            def("everything__echo", "Echoes back the input.", None),
            def("everything__get-sum", "Adds two numbers.", Some(true)),
            def("plan_and_execute", "Plan and execute a task.", None),
        ];
        assert_eq!(
            tool_listing_as_json(&defs),
            json!({
                "tools": [
                    {
                        "name": "everything__echo",
                        "source": "everything",
                        "description": "Echoes back the input.",
                        "readOnly": null,
                        "hasOutputSchema": false,
                    },
                    {
                        "name": "everything__get-sum",
                        "source": "everything",
                        "description": "Adds two numbers.",
                        "readOnly": true,
                        "hasOutputSchema": false,
                    },
                    {
                        "name": "plan_and_execute",
                        "source": "(core)",
                        "description": "Plan and execute a task.",
                        "readOnly": null,
                        "hasOutputSchema": false,
                    },
                ],
                "count": 3,
                "sources": [
                    {"source": "everything", "count": 2},
                    {"source": "(core)", "count": 1},
                ],
            })
        );
    }

    #[test]
    fn json_keeps_multi_line_descriptions_whole() {
        // The text renderer truncates to the first line for density; a
        // description is a routing signal, so the machine form keeps it all.
        let defs = vec![def("s__t", "First line.\nSecond line.", None)];
        let listing = tool_listing_as_json(&defs);
        assert_eq!(
            listing["tools"][0]["description"],
            "First line.\nSecond line."
        );
    }

    #[test]
    fn json_reports_an_empty_catalog_as_a_valid_envelope() {
        assert_eq!(
            tool_listing_as_json(&[]),
            json!({"tools": [], "count": 0, "sources": []})
        );
    }

    #[test]
    fn json_flags_a_declared_output_schema() {
        let mut with_schema = def("s__t", "Desc.", None);
        with_schema.output_schema = Some(json!({"type": "object"}));
        let listing = tool_listing_as_json(&[with_schema]);
        assert_eq!(listing["tools"][0]["hasOutputSchema"], true);
    }

    #[test]
    fn color_mode_emphasizes_headers_names_and_dims_descriptions() {
        let defs = vec![def("s__t", "Desc.", Some(true))];
        let rendered = render_tool_listing(&defs, true);
        assert!(rendered.contains("\x1b[1;4ms\x1b[0m"), "{rendered:?}");
        assert!(rendered.contains("\x1b[1mt\x1b[0m"), "{rendered:?}");
        assert!(
            rendered.contains("\x1b[2m[read-only]\x1b[0m"),
            "{rendered:?}"
        );
        assert!(rendered.contains("\x1b[2mDesc.\x1b[0m"), "{rendered:?}");
    }
}
