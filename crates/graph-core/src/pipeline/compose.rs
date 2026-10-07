use super::doc::parse_plan_source;
use super::native_tools::called_tools;
use super::{authoring, prompts, Pipeline, AGENT_TOOL_PREFIX};
use crate::tools::ToolDef;
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const COMPOSE_CONTEXT_TOOL: &str = "builtin__compose_context";

pub const CHECK_PLAN_TOOL: &str = "builtin__check_plan";

pub fn compose_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: COMPOSE_CONTEXT_TOOL.to_string(),
            description: "Prepares composing a whole plan in one call: the tools every step can \
                          use (control steps and always-loaded tools), the tools search found for \
                          the plan's capabilities, deduplicated and described with their schemas, \
                          the step schema, and the template rules."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["found"],
                "properties": {
                    "found": {"type": "array", "items": {"type": "object"}, "description": "One entry per capability: {does, tools}, where tools is what plan__search_tools returned"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["standing_tools", "tools", "step_schema", "templating_rules", "composing_rules", "rounds"],
                "properties": {
                    "standing_tools": {"type": "string"},
                    "tools": {"type": "string"},
                    "step_schema": {"type": "string"},
                    "templating_rules": {"type": "string"},
                    "composing_rules": {"type": "string"},
                    "rounds": {"type": "array", "items": {"type": "integer"}}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: CHECK_PLAN_TOOL.to_string(),
            description: "Parses a plan written as YAML and validates it: structure, templates, \
                          every tool against the catalog, and the composing rules (an agent step \
                          lists its tools; agent__chat is never used). With `patch`, a YAML \
                          mapping of replacement steps (matched by id, new ones placed after the \
                          step named by their `after`), step ids to `remove`, and new \
                          `input_schema` or `output`, applies it to `yaml` first."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["yaml"],
                "properties": {
                    "yaml": {"type": "string", "description": "The plan as YAML"},
                    "patch": {"type": "string", "description": "Optional YAML patch to apply first"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["valid", "problems", "yaml", "plan"],
                "properties": {
                    "valid": {"type": "boolean"},
                    "problems": {"type": "array", "items": {"type": "string"}},
                    "yaml": {"type": "string"},
                    "plan": {"type": ["object", "null"]}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
    ]
}

impl Pipeline {
    pub(super) async fn compose_context(&self, input: Value) -> Result<Value, String> {
        let scope: BTreeSet<String> = input["found"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["tools"].as_array().cloned().unwrap_or_default())
            .filter_map(|tool| match tool {
                Value::String(name) => Some(name),
                other => other["name"].as_str().map(str::to_string),
            })
            .collect();
        let (standing_tools, tools, step_schema) = self.drafting_catalog(&scope).await;
        Ok(json!({
            "standing_tools": standing_tools,
            "tools": tools,
            "step_schema": step_schema,
            "templating_rules": prompts::TEMPLATING_RULES,
            "composing_rules": prompts::COMPOSING_RULES,
            "rounds": [1, 2],
        }))
    }

    pub(super) async fn check_plan(&self, input: Value) -> Result<Value, String> {
        let yaml = strip_fences(input["yaml"].as_str().unwrap_or_default());
        let yaml = match input["patch"].as_str().map(strip_fences) {
            Some(patch) if !patch.trim().is_empty() => match apply_patch(&yaml, &patch) {
                Ok(merged) => merged,
                Err(problem) => return Ok(rejected(&yaml, vec![problem])),
            },
            _ => yaml,
        };
        let doc = match parse_plan_source(&yaml, "plan") {
            Ok(doc) => doc,
            Err(error) => {
                return Ok(rejected(
                    &yaml,
                    vec![format!("the YAML is not a valid plan: {error}")],
                ))
            }
        };
        let mut problems: Vec<String> =
            authoring::plan_problems(&doc, &self.plans, self.catalog.as_deref())
                .into_iter()
                .filter(|problem| !problem.starts_with("note:"))
                .collect();
        problems.extend(composing_problems(&json!(doc.steps)));
        problems.extend(unused_steps(&doc));
        let defs = self.planner_tool_defs().await;
        problems.extend(missing_inputs(&json!(doc.steps), &defs));
        let declared = doc
            .input_schema
            .as_ref()
            .and_then(|schema| schema.get("properties"))
            .cloned()
            .unwrap_or(Value::Null);
        problems.extend(mistyped_inputs(&json!(doc.steps), &defs, &declared));
        let shapes = self.shapes().await;
        problems.extend(unknown_fields(&doc, &defs, &shapes));
        let plan = authoring::to_json(&doc).map_err(|e| e.to_string())?;
        Ok(json!({
            "valid": problems.is_empty(),
            "problems": problems,
            "yaml": yaml,
            "plan": plan,
        }))
    }
}

fn rejected(yaml: &str, problems: Vec<String>) -> Value {
    json!({ "valid": false, "problems": problems, "yaml": yaml, "plan": null })
}

fn strip_fences(text: &str) -> String {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("```yaml")
        .or_else(|| trimmed.strip_prefix("```yml"))
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.strip_suffix("```").unwrap_or(rest))
        .unwrap_or(trimmed);
    format!("{}\n", body.trim())
}

fn composing_problems(steps: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let chat = format!("{AGENT_TOOL_PREFIX}{}", crate::agent::doc::CHAT_AGENT);
    if called_tools(steps).contains(&chat) {
        problems.push(format!(
            "{chat} is not available to plans; call the tools the step needs"
        ));
    }
    let mut stack = vec![steps];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                let tool = map
                    .get("tool_name")
                    .or_else(|| map.get("toolName"))
                    .and_then(Value::as_str);
                let gate = match tool {
                    Some("exit") => Some("when"),
                    Some("route") => Some("if"),
                    Some("filter") => Some("where"),
                    _ => None,
                };
                if let Some(condition) = gate.and_then(|key| map["input"].get(key)) {
                    if let Err(error) =
                        serde_json::from_value::<super::condition::Condition>(condition.clone())
                    {
                        let id = map.get("id").and_then(Value::as_str).unwrap_or("a gate");
                        problems.push(format!(
                            "step {id}: invalid condition ({error}); `op` is one of eq, ne, gt, lt, gte, lte, empty, not_empty, contains, and a condition is exactly {{value, op, to}}"
                        ));
                    }
                }
                if tool == Some("agent") {
                    let listed = map["input"]["tools"]
                        .as_array()
                        .is_some_and(|tools| !tools.is_empty());
                    if !listed {
                        let id = map
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("an agent step");
                        problems.push(format!(
                            "step {id}: an agent step must list the tools it may call; when the listed tools can do the work in fixed steps, call them directly instead"
                        ));
                    }
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    problems
}

fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(map) => map.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

fn step_tools(value: &Value, out: &mut std::collections::BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            if let (Some(id), Some(tool)) = (
                map.get("id").and_then(Value::as_str),
                map.get("tool_name")
                    .or_else(|| map.get("toolName"))
                    .and_then(Value::as_str),
            ) {
                out.insert(id.to_string(), tool.to_string());
            }
            map.values().for_each(|item| step_tools(item, out));
        }
        Value::Array(items) => items.iter().for_each(|item| step_tools(item, out)),
        _ => {}
    }
}

fn unknown_fields(
    doc: &super::doc::PlanDoc,
    defs: &[ToolDef],
    shapes: &std::collections::HashMap<String, crate::store::ToolShape>,
) -> Vec<String> {
    let steps = json!(doc.steps);
    let mut tools = std::collections::BTreeMap::new();
    step_tools(&steps, &mut tools);
    let mut texts = Vec::new();
    strings(&steps, &mut texts);
    let output = json!(doc.output);
    strings(&output, &mut texts);
    let mut problems = Vec::new();
    for text in texts {
        let Ok(paths) = crate::template::referenced_paths(text) else {
            continue;
        };
        for path in paths {
            let mut segments = path.split('.');
            let (Some(root), Some(field)) = (segments.next(), segments.next()) else {
                continue;
            };
            let Some(tool) = tools.get(root) else {
                continue;
            };
            if matches!(
                tool.as_str(),
                "route" | "map" | "filter" | "reduce" | "exit" | "ask" | "agent"
            ) {
                continue;
            }
            let schema = defs
                .iter()
                .find(|def| &def.name == tool)
                .and_then(|def| def.output_schema.clone())
                .or_else(|| shapes.get(tool).map(|shape| shape.schema.clone()));
            let Some(properties) = schema
                .as_ref()
                .and_then(|schema| schema.get("properties"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            if field == "length" || properties.contains_key(field) {
                continue;
            }
            let known: Vec<&str> = properties.keys().map(String::as_str).collect();
            let problem = format!(
                "`{{{{{path}}}}}`: {tool}'s output has no `{field}`; its fields are {}",
                known.join(", ")
            );
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        }
    }
    problems
}

fn unused_steps(doc: &super::doc::PlanDoc) -> Vec<String> {
    let output = json!(doc.output);
    let solver = json!(doc.solver);
    let mut problems = Vec::new();
    for (index, step) in doc.steps.iter().enumerate() {
        if matches!(step.tool_name.as_str(), "exit" | "ask") {
            continue;
        }
        let later = json!(&doc.steps[index + 1..]).to_string();
        let pattern = format!("{{{{{}", step.id);
        let referenced = |text: &str| {
            text.match_indices(&pattern).any(|(at, _)| {
                text[at + pattern.len()..]
                    .chars()
                    .next()
                    .is_some_and(|next| matches!(next, '.' | '}' | ' ' | '|'))
            })
        };
        if !referenced(&later)
            && !referenced(&output.to_string())
            && !referenced(&solver.to_string())
        {
            problems.push(format!(
                "step {}: its result is never used by a later step or the output; remove it or use it",
                step.id
            ));
        }
    }
    problems
}

fn missing_inputs(steps: &Value, defs: &[ToolDef]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut stack = vec![steps];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                let tool = map
                    .get("tool_name")
                    .or_else(|| map.get("toolName"))
                    .and_then(Value::as_str);
                if let Some(def) = tool.and_then(|tool| defs.iter().find(|def| def.name == tool)) {
                    let given = map.get("input").and_then(Value::as_object);
                    let missing: Vec<&str> = def.input_schema["required"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter(|key| !given.is_some_and(|given| given.contains_key(*key)))
                        .collect();
                    if !missing.is_empty() {
                        let id = map.get("id").and_then(Value::as_str).unwrap_or(&def.name);
                        problems.push(format!(
                            "step {id}: {} requires {}",
                            def.name,
                            missing.join(", ")
                        ));
                    }
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    problems
}

fn json_types(schema: &Value) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.clone()],
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn compatible(given: &[String], wanted: &[String]) -> bool {
    if given.is_empty() || wanted.is_empty() {
        return true;
    }
    given.iter().any(|kind| {
        wanted.contains(kind) || (kind == "integer" && wanted.iter().any(|want| want == "number"))
    })
}

fn mistyped_inputs(steps: &Value, defs: &[ToolDef], declared: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let mut stack = vec![steps];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                let tool = map
                    .get("tool_name")
                    .or_else(|| map.get("toolName"))
                    .and_then(Value::as_str);
                if let (Some(def), Some(given)) = (
                    tool.and_then(|tool| defs.iter().find(|def| def.name == tool)),
                    map.get("input").and_then(Value::as_object),
                ) {
                    for (key, arg) in given {
                        let Some(name) = arg
                            .as_str()
                            .and_then(|text| text.trim().strip_prefix("{{input."))
                            .and_then(|rest| rest.strip_suffix("}}"))
                        else {
                            continue;
                        };
                        let have = json_types(&declared[name]);
                        let want = json_types(&def.input_schema["properties"][key]);
                        if !compatible(&have, &want) {
                            let id = map.get("id").and_then(Value::as_str).unwrap_or(&def.name);
                            problems.push(format!(
                                "step {id}: input `{name}` is {} but {}'s `{key}` takes {}",
                                have.join(" or "),
                                def.name,
                                want.join(" or ")
                            ));
                        }
                    }
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    problems
}

fn apply_patch(yaml: &str, patch: &str) -> Result<String, String> {
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| format!("the plan YAML does not parse: {e}"))?;
    let patch: serde_yaml::Value =
        serde_yaml::from_str(patch).map_err(|e| format!("the patch YAML does not parse: {e}"))?;
    let Some(patch) = patch.as_mapping() else {
        return Err("the patch must be a YAML mapping".to_string());
    };
    let Some(doc_map) = doc.as_mapping_mut() else {
        return Err("the plan must be a YAML mapping".to_string());
    };
    for key in ["input_schema", "output", "name", "description"] {
        if let Some(value) = patch.get(key) {
            doc_map.insert(key.into(), value.clone());
        }
    }
    let steps = doc_map
        .entry("steps".into())
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));
    let Some(steps) = steps.as_sequence_mut() else {
        return Err("the plan's steps must be a list".to_string());
    };
    let id_of = |step: &serde_yaml::Value| {
        step.get("id")
            .and_then(|id| id.as_str())
            .map(str::to_string)
    };
    if let Some(remove) = patch.get("remove").and_then(|remove| remove.as_sequence()) {
        let remove: Vec<&str> = remove.iter().filter_map(|id| id.as_str()).collect();
        steps.retain(|step| !id_of(step).is_some_and(|id| remove.contains(&id.as_str())));
    }
    for replacement in patch
        .get("steps")
        .and_then(|steps| steps.as_sequence())
        .cloned()
        .unwrap_or_default()
    {
        let mut replacement = replacement;
        let after = replacement
            .as_mapping_mut()
            .and_then(|map| map.remove("after"))
            .and_then(|after| after.as_str().map(str::to_string));
        let id = id_of(&replacement);
        match id.and_then(|id| {
            steps
                .iter()
                .position(|step| id_of(step).as_deref() == Some(id.as_str()))
        }) {
            Some(index) => steps[index] = replacement,
            None => {
                let index = after
                    .and_then(|after| {
                        steps
                            .iter()
                            .position(|step| id_of(step).as_deref() == Some(after.as_str()))
                    })
                    .map(|index| index + 1)
                    .unwrap_or(steps.len());
                steps.insert(index, replacement);
            }
        }
    }
    serde_yaml::to_string(&doc).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "version: 2\nidentifier: p\nname: p\ndescription: d\nsteps:\n  - id: a\n    tool_name: t__search\n    input: {query: x}\n  - id: b\n    tool_name: t__issues\n    input: {teamId: \"{{a.values}}\"}\n";

    #[test]
    fn a_patch_replaces_steps_by_id_inserts_after_and_removes() {
        let patch = "steps:\n  - id: b\n    tool_name: t__issues\n    input: {teamId: fixed}\n  - id: c\n    after: a\n    tool_name: t__search\n    input: {query: y}\nremove: []\noutput: {issues: \"{{b}}\"}\n";
        let merged: serde_yaml::Value =
            serde_yaml::from_str(&apply_patch(PLAN, patch).unwrap()).unwrap();
        let ids: Vec<&str> = merged["steps"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|step| step["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "c", "b"]);
        assert_eq!(
            merged["steps"][2]["input"]["teamId"].as_str(),
            Some("fixed")
        );
        assert!(merged["steps"][1].get("after").is_none());
        assert_eq!(merged["output"]["issues"].as_str(), Some("{{b}}"));
    }

    #[test]
    fn a_step_missing_a_required_input_is_a_problem() {
        let defs = vec![ToolDef {
            name: "t__changed".to_string(),
            description: String::new(),
            input_schema: json!({"type": "object", "required": ["base", "head", "prefix"]}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }];
        let steps = json!([
            {"id": "a", "tool_name": "t__changed", "input": {"base": "x", "head": "y"}},
            {"id": "b", "tool_name": "map", "input": {"over": "[]", "do": {"tool_name": "t__changed", "input": {"base": "x", "head": "y", "prefix": ""}}}},
        ]);
        assert_eq!(
            missing_inputs(&steps, &defs),
            ["step a: t__changed requires prefix"]
        );
    }

    #[test]
    fn a_plan_input_passed_whole_to_a_parameter_of_another_type_is_a_problem() {
        let defs = vec![ToolDef {
            name: "t__changed".to_string(),
            description: String::new(),
            input_schema: json!({"type": "object", "properties": {"base": {"type": "string"}, "count": {"type": "number"}}}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }];
        let declared = json!({"pr": {"type": "integer"}, "n": {"type": "integer"}});
        let steps = json!([
            {"id": "a", "tool_name": "t__changed", "input": {"base": "{{input.pr}}", "count": "{{input.n}}"}},
            {"id": "b", "tool_name": "t__changed", "input": {"base": "pr-{{input.pr}}"}},
        ]);
        assert_eq!(
            mistyped_inputs(&steps, &defs, &declared),
            ["step a: input `pr` is integer but t__changed's `base` takes string"]
        );
    }

    #[test]
    fn an_unknown_gate_operator_is_a_problem() {
        let steps = json!([
            {"id": "kept", "tool_name": "filter", "input": {"over": "{{a.items}}", "where": {"value": "{{item.path}}", "op": "not_contains", "to": "docs/"}}},
            {"id": "fine", "tool_name": "filter", "input": {"over": "{{a.items}}", "where": {"value": "{{item.path}}", "op": "contains", "to": "docs/"}}},
            {"id": "stop", "tool_name": "exit", "input": {"when": {"value": "{{a.count}}", "op": "eq", "to": 0}, "status": "success"}},
        ]);
        let problems = composing_problems(&steps);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("step kept: invalid condition"));
    }

    #[test]
    fn a_step_nothing_reads_is_a_problem() {
        let yaml = "version: 2\nidentifier: p\nname: p\ndescription: d\nsteps:\n  - id: a\n    tool_name: t__search\n    input: {query: x}\n  - id: ab\n    tool_name: t__search\n    input: {query: y}\n  - id: b\n    tool_name: t__issues\n    input: {teamId: \"{{a.values}}\"}\noutput: {issues: \"{{b}}\"}\n";
        let doc = parse_plan_source(yaml, "plan").unwrap();
        assert_eq!(
            unused_steps(&doc),
            ["step ab: its result is never used by a later step or the output; remove it or use it"]
        );
    }

    #[test]
    fn a_field_the_tool_does_not_return_is_a_problem() {
        let yaml = "version: 2\nidentifier: p\nname: p\ndescription: d\nsteps:\n  - id: issue\n    tool_name: t__issue\n    input: {id: x}\n  - id: pick\n    tool_name: route\n    input:\n      if: {value: \"{{issue.title}}\", op: not_empty}\n      then: {tool_name: t__issue, input: {id: y}}\n      else: {tool_name: t__issue, input: {id: z}}\noutput: {a: \"{{issue.project}}\", b: \"{{issue.title}}\", c: \"{{pick.result.items}}\", d: \"{{pick.result}}\"}\n";
        let doc = parse_plan_source(yaml, "plan").unwrap();
        let defs = vec![ToolDef {
            name: "t__issue".to_string(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            output_schema: Some(
                json!({"type": "object", "properties": {"title": {}, "status": {}}}),
            ),
            output_example: None,
            read_only: None,
        }];
        let problems = unknown_fields(&doc, &defs, &Default::default());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("t__issue's output has no `project`"));
    }

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```yaml\nversion: 2\n```"), "version: 2\n");
        assert_eq!(strip_fences("version: 2"), "version: 2\n");
    }

    #[test]
    fn agent_steps_must_list_tools_and_chat_is_refused() {
        let steps = json!([
            {"id": "x", "tool_name": "agent", "input": {"prompt": "do it"}},
            {"id": "y", "tool_name": "agent", "input": {"prompt": "do it", "tools": ["t__search"]}},
            {"id": "z", "tool_name": "agent__chat", "input": {"prompt": "do it"}},
        ]);
        let problems = composing_problems(&steps);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("step x")));
        assert!(problems.iter().any(|p| p.contains("agent__chat")));
    }
}
