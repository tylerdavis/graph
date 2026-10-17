use super::content::{apply_fields, content, Content};
use super::mdx::{
    attr, frontmatter, has_fields, paragraphs, scalar, schema_fields, schema_from_example,
    text_line, value_field, Field,
};
use super::Section;
use graph_core::tools::ToolDef;
use graph_core::user_tools::builtin_tool_sources;
use serde_json::{json, Value};
use serde_yaml::Value as Yaml;
use std::collections::BTreeMap;
use std::fmt::Write as _;

const BUILTIN_DIR: &str = "tools/reference/builtin";
const STEP_DIR: &str = "plans/reference/steps";
const AGENT_DIR: &str = "agents/reference/built-in";
const PLAN_DIR: &str = "plans/reference/built-in";
const NATIVE_DIR: &str = "tools/reference/native";
const WORKBENCH_TOOL_DIR: &str = "tools/reference/workbench";
const MCP_TOOL_DIR: &str = "integrating/mcp/reference/server-tools";

const BUILTIN_ORDER: &[&str] = &["infer", "decide", "reshape"];
const STEP_ORDER: &[&str] = &["route", "filter", "map", "reduce", "exit", "agent", "ask"];
const PLAN_ORDER: &[&str] = &["compose_plan", "search_tools"];
const GLOBAL_AGENT_ORDER: &[&str] = &["chat", "agent_drafter", "tool_drafter"];
const WORKBENCH_AGENT_ORDER: &[&str] = &[
    "front_desk",
    "plan_drafter",
    "plan_loader",
    "plan_editor",
    "agent_drafter",
    "tool_drafter",
];
const NATIVE_GROUPS: &[(&str, &[&str])] = &[
    (
        "Plan authoring",
        &[
            "builtin__validate_plan",
            "builtin__check_plan",
            "builtin__compose_context",
            "builtin__question_form",
            "builtin__find_tools",
        ],
    ),
    (
        "Tool search",
        &[
            "builtin__catalog_tools",
            "builtin__score_candidates",
            "builtin__describe_tools",
        ],
    ),
    (
        "Drafting",
        &[
            "builtin__try_tool",
            "builtin__save_tool",
            "builtin__save_agent",
        ],
    ),
];
const WORKBENCH_TOOL_GROUPS: &[(&str, &[&str])] = &[
    (
        "Drafts",
        &[
            "workbench__list_artifacts",
            "workbench__show_artifact",
            "workbench__load_artifact",
            "workbench__set_artifact",
            "workbench__try_artifact",
            "workbench__save_artifact",
            "workbench__discard_artifact",
        ],
    ),
    (
        "Plan edits",
        &[
            "workbench__update_metadata",
            "workbench__add_step",
            "workbench__update_step",
            "workbench__delete_step",
            "workbench__restore_draft",
        ],
    ),
    ("Catalog", &["workbench__describe_tool"]),
    (
        "Files",
        &["workbench__read_file", "workbench__glob", "workbench__grep"],
    ),
];
const MCP_TOOL_GROUPS: &[(&str, &[&str])] = &[(
    "",
    &[
        "graph_plan_list",
        "graph_plan_show",
        "graph_plan_new",
        "graph_plan_draft",
        "graph_plan_set",
        "graph_plan_unset",
        "graph_plan_step_add",
        "graph_plan_step_update",
        "graph_plan_step_rename",
        "graph_plan_step_rm",
        "graph_plan_validate",
        "graph_tools_list",
        "graph_tools_show",
        "graph_tools_test",
    ],
)];

pub fn sections() -> Vec<Section> {
    vec![
        builtin_tools(),
        steps(),
        agents(),
        plans(),
        native_tools(),
        workbench_tools(),
        mcp_server_tools(),
    ]
}

fn rank(name: &str, order: &[&str]) -> usize {
    order
        .iter()
        .position(|candidate| *candidate == name)
        .unwrap_or(order.len())
}

fn short_name(name: &str) -> &str {
    name.split_once("__").map(|(_, rest)| rest).unwrap_or(name)
}

fn nav_file(dir: &str) -> String {
    format!("{dir}/navigation.json")
}

fn nav_json(group: &str, root: Option<&str>, pages: Vec<Value>) -> String {
    let mut out = serde_json::to_string_pretty(&nav_group(group, root, pages)).unwrap();
    out.push('\n');
    out
}

fn nav_group(group: &str, root: Option<&str>, pages: Vec<Value>) -> Value {
    match root {
        Some(root) => json!({ "group": group, "root": root, "pages": pages }),
        None => json!({ "group": group, "pages": pages }),
    }
}

fn yaml(raw: &str) -> Yaml {
    serde_yaml::from_str(raw).expect("built-in YAML parses")
}

fn text<'a>(doc: &'a Yaml, key: &str) -> &'a str {
    doc.get(key).and_then(Yaml::as_str).unwrap_or_default()
}

fn strings(doc: &Yaml, key: &str) -> Vec<String> {
    doc.get(key)
        .and_then(Yaml::as_sequence)
        .map(|items| {
            items
                .iter()
                .filter_map(Yaml::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn to_yaml(value: &Value) -> Yaml {
    serde_yaml::to_value(value).unwrap_or(Yaml::Null)
}

fn panel(out: &mut String, tag: &str, blocks: &[(&str, &str, String)]) {
    if blocks.is_empty() {
        return;
    }
    let _ = writeln!(out, "<{tag}>\n");
    for (language, title, code) in blocks {
        let _ = writeln!(out, "```{language} {title}\n{}\n```\n", code.trim_end());
    }
    let _ = writeln!(out, "</{tag}>\n");
}

fn finish(mut out: String) -> String {
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

fn step_yaml(tool: &str, schema: Option<&Yaml>) -> String {
    let mut out = format!("- id: E1\n  tool_name: {tool}\n  input:");
    let required: Vec<String> = schema
        .map(|schema| strings(schema, "required"))
        .unwrap_or_default();
    if required.is_empty() {
        out.push_str(" {}");
    }
    for name in required {
        let _ = write!(out, "\n    {name}: \"{{{{input.{name}}}}}\"");
    }
    out
}

fn page_head(out: &mut String, title: &str, sidebar: &str, doc: &Content, keywords: &[&str]) {
    frontmatter(
        out,
        &[
            ("title", title.to_string()),
            ("sidebarTitle", sidebar.to_string()),
            ("description", doc.summary.clone()),
            ("playground", "none".to_string()),
        ],
        keywords,
    );
    if !doc.body.is_empty() {
        let _ = writeln!(out, "{}\n", doc.body);
    }
}

fn input_section(out: &mut String, schema: &Yaml) {
    out.push_str("## Input\n\n");
    if schema
        .get("properties")
        .and_then(Yaml::as_mapping)
        .is_some_and(|props| !props.is_empty())
    {
        schema_fields(out, schema, Field::Param, "");
    } else {
        out.push_str("Takes no input.\n\n");
    }
}

fn output_section(
    out: &mut String,
    heading: &str,
    intro: &str,
    schema: Option<Yaml>,
    doc: &Content,
) {
    let Some(mut schema) = schema else { return };
    apply_fields(&mut schema, &doc.fields);
    if !has_fields(&schema) {
        return;
    }
    let _ = writeln!(out, "## {heading}\n");
    if !intro.is_empty() {
        let _ = writeln!(out, "{intro}\n");
    }
    schema_fields(out, &schema, Field::Response, "");
}

fn index_page(title: &str, doc: &Content, rows: &[(String, String, String)]) -> String {
    let mut out = String::new();
    frontmatter(
        &mut out,
        &[
            ("title", title.to_string()),
            ("description", doc.summary.clone()),
        ],
        &[title],
    );
    if !doc.body.is_empty() {
        let _ = writeln!(out, "{}\n", doc.body);
    }
    out.push_str("| Name | Description |\n| --- | --- |\n");
    for (link, name, summary) in rows {
        let _ = writeln!(out, "| [`{name}`](/{link}) | {} |", text_line(summary));
    }
    out
}

fn builtin_tools() -> Section {
    let mut files = BTreeMap::new();
    let mut pages = Vec::new();
    let mut rows = Vec::new();
    let mut docs: Vec<Yaml> = builtin_tool_sources().iter().map(|raw| yaml(raw)).collect();
    docs.sort_by_key(|doc| rank(text(doc, "name"), BUILTIN_ORDER));
    for doc in docs {
        let name = text(&doc, "name");
        let path = format!("{BUILTIN_DIR}/{name}");
        let words = content(&format!("builtin-tools/{name}"), text(&doc, "description"));
        files.insert(format!("{path}.mdx"), tool_page(&doc, &words));
        rows.push((
            path.clone(),
            format!("builtin__{name}"),
            words.summary.clone(),
        ));
        pages.push(json!(path));
    }
    let overview = content(
        "sections/builtin-tools",
        "Built-in tools are always loaded, called as `builtin__<name>`.",
    );
    files.insert(
        "tools/reference/builtin-tools.mdx".to_string(),
        index_page("Built-in tools", &overview, &rows),
    );
    files.insert(
        nav_file(BUILTIN_DIR),
        nav_json(
            "Built-in tools",
            Some("tools/reference/builtin-tools"),
            pages,
        ),
    );
    Section {
        dir: Some(BUILTIN_DIR),
        nav: Some(nav_file(BUILTIN_DIR)),
        hand_written: &[],
        files,
    }
}

fn tool_page(doc: &Yaml, words: &Content) -> String {
    let name = text(doc, "name");
    let tool = format!("builtin__{name}");
    let mut out = String::new();
    page_head(&mut out, &tool, &tool, words, &[tool.as_str()]);
    let read_only = doc.get("read_only").and_then(Yaml::as_bool);
    let kind = text(doc, "kind");
    let effect = match (kind, read_only) {
        (_, Some(false)) => "Has side effects",
        ("reshape", _) | (_, Some(true)) => "Read-only",
        _ => "Not marked read-only",
    };
    let _ = writeln!(
        out,
        "| Loaded | Kind | Effect |\n| --- | --- | --- |\n| Always | `{kind}` | {effect} |\n"
    );
    input_section(&mut out, doc.get("input_schema").unwrap_or(&Yaml::Null));
    match doc.get("output_schema").or(words.output.as_ref()).cloned() {
        Some(schema) if !has_fields(&schema) => {
            out.push_str("## Output\n\n");
            value_field(&mut out, &schema);
        }
        output => {
            let intro = output
                .as_ref()
                .and_then(|schema| schema.get("description"))
                .and_then(Yaml::as_str)
                .map(paragraphs)
                .unwrap_or_default();
            output_section(&mut out, "Output", &intro, output, words);
        }
    }
    panel(
        &mut out,
        "RequestExample",
        &[
            (
                "yaml",
                "Plan step",
                step_yaml(&tool, doc.get("input_schema")),
            ),
            ("bash", "Inspect", format!("graph tool show {tool}")),
        ],
    );
    finish(out)
}

fn steps() -> Section {
    let mut files = BTreeMap::new();
    let mut nav = Vec::new();
    let mut rows = Vec::new();
    let mut defs = graph_core::pipeline::control_step_defs();
    defs.sort_by_key(|def| rank(&def.name, STEP_ORDER));
    for def in defs {
        let path = format!("{STEP_DIR}/{}", def.name);
        let words = content(&format!("steps/{}", def.name), &def.description);
        files.insert(format!("{path}.mdx"), step_page(&def, &words));
        rows.push((path.clone(), def.name.clone(), words.summary.clone()));
        nav.push(json!(path));
    }
    let overview = content(
        "sections/steps",
        "Steps graph runs itself instead of calling a tool.",
    );
    files.insert(
        "plans/reference/control-steps.mdx".to_string(),
        index_page("Control steps", &overview, &rows),
    );
    files.insert(
        nav_file(STEP_DIR),
        nav_json("Control steps", Some("plans/reference/control-steps"), nav),
    );
    Section {
        dir: Some(STEP_DIR),
        nav: Some(nav_file(STEP_DIR)),
        hand_written: &[],
        files,
    }
}

fn step_page(def: &ToolDef, words: &Content) -> String {
    let mut out = String::new();
    page_head(
        &mut out,
        &def.name,
        &def.name,
        words,
        &[def.name.as_str(), "control step"],
    );
    let input = to_yaml(&def.input_schema);
    input_section(&mut out, &input);
    let result = def.output_schema.as_ref().map(to_yaml).or_else(|| {
        def.output_example
            .as_ref()
            .map(|example| schema_from_example(&to_yaml(example)))
    });
    output_section(
        &mut out,
        "Result",
        "Later steps read these as `{{<step id>.<field>}}`.",
        result,
        words,
    );
    panel(
        &mut out,
        "RequestExample",
        &[("yaml", "Plan step", step_yaml(&def.name, Some(&input)))],
    );
    if let Some(example) = &def.output_example {
        panel(
            &mut out,
            "ResponseExample",
            &[(
                "json",
                "Result",
                serde_json::to_string_pretty(example).unwrap(),
            )],
        );
    }
    finish(out)
}

fn agents() -> Section {
    let mut files = BTreeMap::new();
    let mut rows = Vec::new();
    let mut global = Vec::new();
    let mut globals: Vec<Yaml> = graph_core::agent::doc::BUILTINS
        .iter()
        .map(|raw| yaml(raw))
        .collect();
    globals.sort_by_key(|doc| rank(text(doc, "name"), GLOBAL_AGENT_ORDER));
    for doc in globals {
        let name = text(&doc, "name");
        let path = format!("{AGENT_DIR}/{name}");
        let words = content(&format!("agents/{name}"), text(&doc, "description"));
        files.insert(format!("{path}.mdx"), agent_page(&doc, false, &words));
        rows.push((path.clone(), name.to_string(), words.summary.clone()));
        global.push(json!(path));
    }
    let mut workbench = Vec::new();
    let mut workbench_docs: Vec<Yaml> = crate::workbench::agents::WORKBENCH_AGENTS
        .iter()
        .map(|raw| yaml(raw))
        .collect();
    workbench_docs.sort_by_key(|doc| rank(text(doc, "name"), WORKBENCH_AGENT_ORDER));
    for doc in workbench_docs {
        let name = text(&doc, "name");
        let path = format!("{AGENT_DIR}/workbench/{name}");
        let words = content(
            &format!("workbench-agents/{name}"),
            text(&doc, "description"),
        );
        files.insert(format!("{path}.mdx"), agent_page(&doc, true, &words));
        rows.push((
            path.clone(),
            format!("{name} (workbench)"),
            words.summary.clone(),
        ));
        workbench.push(json!(path));
    }
    let overview = content("sections/agents", "The agents that ship with graph.");
    files.insert(
        "agents/reference/built-in-agents.mdx".to_string(),
        index_page("Built-in agents", &overview, &rows),
    );
    files.insert(
        nav_file(AGENT_DIR),
        nav_json(
            "Built-in agents",
            Some("agents/reference/built-in-agents"),
            vec![
                nav_group("Global", None, global),
                nav_group("Workbench", None, workbench),
            ],
        ),
    );
    Section {
        dir: Some(AGENT_DIR),
        nav: Some(nav_file(AGENT_DIR)),
        hand_written: &[],
        files,
    }
}

fn agent_page(doc: &Yaml, workbench: bool, words: &Content) -> String {
    let name = text(doc, "name");
    let title = if workbench {
        format!("{name} (workbench)")
    } else {
        name.to_string()
    };
    let mut out = String::new();
    page_head(&mut out, &title, name, words, &[name, "agent"]);
    let default_agent = name == graph_core::agent::doc::CHAT_AGENT;
    let runs = if workbench {
        "`graph wb` only".to_string()
    } else if default_agent {
        "`graph ask` and `graph chat`, as the default agent".to_string()
    } else {
        format!("`graph ask --agent {name}`, `graph chat {name}`, plan steps as `agent__{name}`")
    };
    let max = doc
        .get("max_iterations")
        .map(scalar)
        .unwrap_or_else(|| "default".to_string());
    let _ = writeln!(
        out,
        "| Model role | Runs in | Max iterations |\n| --- | --- | --- |\n| `{}` | {runs} | {max} |\n",
        text(doc, "model")
    );
    list_section(&mut out, "Tools", &strings(doc, "tools"), None);
    let link_base = if workbench {
        format!("/{AGENT_DIR}/workbench")
    } else {
        format!("/{AGENT_DIR}")
    };
    list_section(
        &mut out,
        "Subagents",
        &strings(doc, "subagents"),
        Some(&link_base),
    );
    list_section(
        &mut out,
        "Handoffs",
        &strings(doc, "handoffs"),
        Some(&link_base),
    );
    if let Some(schema) = doc.get("input_schema") {
        out.push_str("## Input\n\n");
        schema_fields(&mut out, schema, Field::Param, "");
    }
    if let Some(schema) = doc.get("output_schema") {
        out.push_str("## Output\n\n");
        schema_fields(&mut out, schema, Field::Response, "");
    }
    out.push_str("## System prompt\n\n<Accordion title=\"Show the system prompt\">\n\n");
    let _ = writeln!(
        out,
        "```markdown\n{}\n```\n",
        text(doc, "system_prompt").trim_end()
    );
    out.push_str("</Accordion>\n\n");
    let mut blocks = vec![(
        "bash",
        "Customize",
        format!("graph agent show {name} > .graph/agents/{name}.yaml"),
    )];
    if default_agent {
        blocks.push(("bash", "Run", "graph chat".to_string()));
    } else if !workbench {
        blocks.push(("bash", "Run", format!("graph chat {name}")));
    }
    panel(&mut out, "RequestExample", &blocks);
    finish(out)
}

fn list_section(out: &mut String, heading: &str, items: &[String], link_base: Option<&str>) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "## {heading}\n");
    for item in items {
        match link_base {
            Some(base) => {
                let _ = writeln!(out, "- [`{item}`]({base}/{item})");
            }
            None => {
                let _ = writeln!(out, "- `{item}`");
            }
        }
    }
    out.push('\n');
}

fn plans() -> Section {
    let mut files = BTreeMap::new();
    let mut nav = Vec::new();
    let mut rows = Vec::new();
    let mut plan_docs: Vec<Yaml> = graph_core::pipeline::doc::BUILTIN_PLANS
        .iter()
        .map(|raw| yaml(raw))
        .collect();
    plan_docs.sort_by_key(|doc| rank(text(doc, "identifier"), PLAN_ORDER));
    for doc in plan_docs {
        let identifier = text(&doc, "identifier");
        let path = format!("{PLAN_DIR}/{identifier}");
        let words = content(&format!("plans/{identifier}"), text(&doc, "description"));
        files.insert(format!("{path}.mdx"), plan_page(&doc, &words));
        rows.push((path.clone(), identifier.to_string(), words.summary.clone()));
        nav.push(json!(path));
    }
    let overview = content("sections/plans", "The plans that ship with graph.");
    files.insert(
        "plans/reference/built-in-plans.mdx".to_string(),
        index_page("Built-in plans", &overview, &rows),
    );
    files.insert(
        nav_file(PLAN_DIR),
        nav_json(
            "Built-in plans",
            Some("plans/reference/built-in-plans"),
            nav,
        ),
    );
    Section {
        dir: Some(PLAN_DIR),
        nav: Some(nav_file(PLAN_DIR)),
        hand_written: &[],
        files,
    }
}

fn plan_page(doc: &Yaml, words: &Content) -> String {
    let identifier = text(doc, "identifier");
    let tool = format!("plan__{identifier}");
    let mut out = String::new();
    page_head(
        &mut out,
        identifier,
        identifier,
        words,
        &[identifier, tool.as_str()],
    );
    input_section(&mut out, doc.get("input_schema").unwrap_or(&Yaml::Null));
    let finish_output = doc
        .get("finish")
        .and_then(|finish| finish.get("output"))
        .and_then(Yaml::as_mapping);
    if let Some(schema) = doc.get("output_schema") {
        output_section(&mut out, "Output", "", Some(schema.clone()), words);
    } else if let Some(output) = finish_output {
        out.push_str("## Output\n\n");
        for (key, _) in output {
            let key = scalar(key);
            let field = words.fields.get(&key);
            let kind = field
                .and_then(|field| field.kind.clone())
                .unwrap_or_else(|| "any".to_string());
            let description = field
                .map(|field| text_line(&field.description))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "<ResponseField name=\"{}\" type=\"{}\">\n  {description}\n</ResponseField>\n",
                attr(&key),
                attr(&kind)
            );
        }
    }
    if let Some(steps) = doc.get("steps").and_then(Yaml::as_sequence) {
        out.push_str("## Steps\n\n| Step | Calls |\n| --- | --- |\n");
        for step in steps {
            let _ = writeln!(
                out,
                "| `{}` | `{}` |",
                text(step, "id"),
                text(step, "tool_name")
            );
        }
        out.push('\n');
    }
    panel(
        &mut out,
        "RequestExample",
        &[
            (
                "yaml",
                "Plan step",
                step_yaml(&tool, doc.get("input_schema")),
            ),
            (
                "bash",
                "Customize",
                format!("graph plan show {identifier} > .graph/plans/{identifier}.yaml"),
            ),
        ],
    );
    finish(out)
}

fn grouped<'a>(
    defs: &'a [ToolDef],
    groups: &[(&'static str, &[&str])],
) -> Vec<(&'static str, Vec<&'a ToolDef>)> {
    let mut out: Vec<(&'static str, Vec<&ToolDef>)> = groups
        .iter()
        .map(|(group, names)| {
            let mut members: Vec<&ToolDef> = defs
                .iter()
                .filter(|def| names.contains(&def.name.as_str()))
                .collect();
            members.sort_by_key(|def| rank(&def.name, names));
            (*group, members)
        })
        .filter(|(_, members)| !members.is_empty())
        .collect();
    let other: Vec<&ToolDef> = defs
        .iter()
        .filter(|def| {
            !groups
                .iter()
                .any(|(_, names)| names.contains(&def.name.as_str()))
        })
        .collect();
    if !other.is_empty() {
        out.push(("Other", other));
    }
    out
}

struct DefSection<'a> {
    dir: &'static str,
    key: &'static str,
    index: &'static str,
    title: &'static str,
    facts: &'static str,
    step_example: bool,
    groups: &'a [(&'static str, &'static [&'static str])],
}

fn def_page(def: &ToolDef, words: &Content, facts: &str, step_example: bool) -> String {
    let mut out = String::new();
    page_head(&mut out, &def.name, &def.name, words, &[def.name.as_str()]);
    let effect = match def.read_only {
        Some(true) => "Read-only",
        Some(false) => "Has side effects",
        None => "Not marked read-only",
    };
    let _ = writeln!(
        out,
        "| Available to | Effect |\n| --- | --- |\n| {facts} | {effect} |\n"
    );
    let input = to_yaml(&def.input_schema);
    input_section(&mut out, &input);
    let output = def.output_schema.as_ref().map(to_yaml).or_else(|| {
        def.output_example
            .as_ref()
            .map(|example| schema_from_example(&to_yaml(example)))
    });
    output_section(&mut out, "Output", "", output, words);
    if step_example {
        panel(
            &mut out,
            "RequestExample",
            &[("yaml", "Plan step", step_yaml(&def.name, Some(&input)))],
        );
    }
    if let Some(example) = &def.output_example {
        panel(
            &mut out,
            "ResponseExample",
            &[(
                "json",
                "Output",
                serde_json::to_string_pretty(example).unwrap(),
            )],
        );
    }
    finish(out)
}

fn def_section(spec: DefSection, defs: Vec<ToolDef>) -> Section {
    let mut files = BTreeMap::new();
    let mut nav = Vec::new();
    let mut rows = Vec::new();
    for (group, members) in grouped(&defs, spec.groups) {
        let mut pages = Vec::new();
        for def in members {
            let path = format!("{}/{}", spec.dir, short_name(&def.name));
            let words = content(&format!("{}/{}", spec.key, def.name), &def.description);
            files.insert(
                format!("{path}.mdx"),
                def_page(def, &words, spec.facts, spec.step_example),
            );
            rows.push((path.clone(), def.name.clone(), words.summary.clone()));
            pages.push(json!(path));
        }
        if group.is_empty() {
            nav.extend(pages);
        } else {
            nav.push(nav_group(group, None, pages));
        }
    }
    let overview = content(&format!("sections/{}", spec.key), spec.title);
    files.insert(
        format!("{}.mdx", spec.index),
        index_page(spec.title, &overview, &rows),
    );
    files.insert(
        nav_file(spec.dir),
        nav_json(spec.title, Some(spec.index), nav),
    );
    Section {
        dir: Some(spec.dir),
        nav: Some(nav_file(spec.dir)),
        hand_written: &[],
        files,
    }
}

fn native_tools() -> Section {
    def_section(
        DefSection {
            dir: NATIVE_DIR,
            key: "native-tools",
            index: "tools/reference/native-tools",
            title: "Native tools",
            facts: "Any agent or plan step",
            step_example: true,
            groups: NATIVE_GROUPS,
        },
        graph_core::pipeline::native_tool_defs(),
    )
}

fn workbench_tools() -> Section {
    let mut defs = crate::workbench::tools::tool_defs();
    defs.extend(crate::workbench::fs_tools::tool_defs());
    def_section(
        DefSection {
            dir: WORKBENCH_TOOL_DIR,
            key: "workbench-tools",
            index: "tools/reference/workbench-tools",
            title: "Workbench tools",
            facts: "Workbench agents only",
            step_example: false,
            groups: WORKBENCH_TOOL_GROUPS,
        },
        defs,
    )
}

fn mcp_server_tools() -> Section {
    let defs = crate::mcp_server::catalog::authoring_tools()
        .into_iter()
        .map(|tool| ToolDef {
            name: tool.name.to_string(),
            description: tool
                .description
                .as_deref()
                .map(str::to_string)
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
            output_schema: None,
            output_example: None,
            read_only: None,
        })
        .collect();
    def_section(
        DefSection {
            dir: MCP_TOOL_DIR,
            key: "mcp-server-tools",
            index: "integrating/mcp/reference/server-tools",
            title: "MCP server tools",
            facts: "MCP clients of `graph mcp serve`",
            step_example: false,
            groups: MCP_TOOL_GROUPS,
        },
        defs,
    )
}
