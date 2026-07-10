//! Shared renderer for grouped tool listings — `graph tools list` and
//! `graph mcp tools`.

use graph_core::ToolDef;
use graph_mcp::NAMESPACE_SEPARATOR;

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
        let source = def
            .name
            .split_once(NAMESPACE_SEPARATOR)
            .map_or("(core)", |(source, _)| source);
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
