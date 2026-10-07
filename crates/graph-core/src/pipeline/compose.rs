use super::doc::parse_plan_source;
use super::native_tools::called_tools;
use super::{authoring, prompts, Pipeline, AGENT_TOOL_PREFIX};

const SEARCH_TOOLS_PLAN: &str = "search_tools";
use crate::tools::ToolDef;
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub fn draft_input(goal: &str) -> Value {
    json!({ "goal": goal })
}

pub const COMPOSE_PLAN: &str = "compose_plan";

pub const COMPOSE_CONTEXT_TOOL: &str = "builtin__compose_context";

pub const CHECK_PLAN_TOOL: &str = "builtin__check_plan";

pub const QUESTION_FORM_TOOL: &str = "builtin__question_form";

pub const FIND_TOOLS_TOOL: &str = "builtin__find_tools";

pub const INSTRUCTIONS_KEY: &str = "instructions";

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
                    "patch": {"type": "string", "description": "Optional YAML patch to apply first"},
                    "search": {"type": "array", "items": {"type": "string"}, "description": "Tools the model asked to be searched for; the plan is not settled while any are pending"},
                    "pending": {"type": "boolean", "description": "Answers that the plan has not taken in yet; the plan is not settled while true"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["valid", "settled", "problems", "yaml", "plan"],
                "properties": {
                    "valid": {"type": "boolean"},
                    "settled": {"type": "boolean", "description": "Valid, with no search requested and no answers pending"},
                    "search": {"type": "array", "items": {"type": "string"}, "description": "The tool searches still requested, including any written into the plan by mistake"},
                    "problems": {"type": "array", "items": {"type": "string"}},
                    "yaml": {"type": "string"},
                    "plan": {"type": ["object", "null"]}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: FIND_TOOLS_TOOL.to_string(),
            description:
                "Searches for tools a drafting call asked for, one search_tools run per \
                          request, and adds what it finds to the tools already found. Returns the \
                          combined `found` list and the tools described for the next drafting call."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["queries", "found"],
                "properties": {
                    "queries": {"type": "array", "items": {"type": "string"}, "description": "What each missing tool should do"},
                    "found": {"type": "array", "items": {"type": "object"}, "description": "The {does, tools} entries found so far"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["found", "tools"],
                "properties": {
                    "found": {"type": "array", "items": {"type": "object"}},
                    "tools": {"type": "string"}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: QUESTION_FORM_TOOL.to_string(),
            description: "Turns the questions a draft asked into an ask step's answer schema: \
                          one string field per question (a pick list when it has options), its \
                          question as the field's description, plus an open `instructions` field \
                          for anything else the user wants the planner to know. The draft's \
                          guesses become the default answer."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["draft"],
                "properties": {
                    "draft": {"type": "object", "description": "The draft call's result; its `questions` are read when present: [{key, question, guess?, options?}]"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["count", "questions", "schema", "default"],
                "properties": {
                    "count": {"type": "integer"},
                    "questions": {"type": "array", "items": {"type": "object"}},
                    "schema": {"type": "object"},
                    "default": {"type": "object"}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
    ]
}

pub(super) fn question_form(input: Value) -> Result<Value, String> {
    let mut asked = input["draft"]["questions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if asked.is_empty() {
        let yaml = strip_fences(input["draft"]["yaml"].as_str().unwrap_or_default());
        asked = split_key(&yaml, "questions").1;
    }
    let mut questions = Vec::new();
    let mut properties = serde_json::Map::new();
    let mut default = serde_json::Map::new();
    for question in &asked {
        let (Some(key), Some(text)) = (question["key"].as_str(), question["question"].as_str())
        else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key == INSTRUCTIONS_KEY || properties.contains_key(key) {
            continue;
        }
        let options: Vec<&str> = question["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut property = json!({"type": "string", "description": text});
        if !options.is_empty() {
            property["enum"] = json!(options);
        }
        if let Some(guess) = question["guess"].as_str() {
            if options.is_empty() || options.contains(&guess) {
                default.insert(key.to_string(), json!(guess));
            }
        }
        properties.insert(key.to_string(), property);
        questions.push(question.clone());
    }
    if !properties.is_empty() {
        properties.insert(
            INSTRUCTIONS_KEY.to_string(),
            json!({"type": "string", "description": "Anything else the planner should know?"}),
        );
    }
    let mut order: Vec<String> = questions
        .iter()
        .filter_map(|question| question["key"].as_str())
        .map(|key| key.trim().to_string())
        .collect();
    if !order.is_empty() {
        order.push(INSTRUCTIONS_KEY.to_string());
    }
    Ok(json!({
        "count": questions.len(),
        "questions": questions,
        "schema": {"type": "object", "properties": properties, "propertyOrder": order},
        "default": default,
    }))
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
            "rounds": [1, 2, 3],
        }))
    }

    pub(super) async fn find_tools(&self, input: Value) -> Result<Value, String> {
        let mut found = input["found"].as_array().cloned().unwrap_or_default();
        let queries: Vec<String> = input["queries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_string)
            .collect();
        let searches = futures::future::join_all(queries.iter().map(|query| {
            self.call_plan(SEARCH_TOOLS_PLAN, json!({"query": query, "schemas": false}))
        }))
        .await;
        for (query, search) in queries.iter().zip(searches) {
            if !search.is_error {
                found.push(json!({"does": query, "tools": search.result["tools"]}));
            }
        }
        let context = self.compose_context(json!({ "found": found })).await?;
        Ok(json!({"found": found, "tools": context["tools"]}))
    }

    pub(super) async fn check_plan(&self, input: Value) -> Result<Value, String> {
        let yaml = strip_fences(input["yaml"].as_str().unwrap_or_default());
        let (yaml, _) = split_key(&yaml, "questions");
        let (yaml, inline_search) = split_key(&yaml, "search");
        let search: Vec<String> = input["search"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(&inline_search)
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_string)
            .collect();
        let mut checked = self.checked_plan(&yaml, &input).await?;
        let pending = input["pending"].as_bool().unwrap_or(false);
        let settled = checked["valid"] == json!(true) && search.is_empty() && !pending;
        checked["settled"] = json!(settled);
        checked["search"] = json!(search);
        Ok(checked)
    }

    async fn checked_plan(&self, yaml: &str, input: &Value) -> Result<Value, String> {
        let yaml = yaml.to_string();
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
            authoring::plan_problems(&doc, &self.plans, self.live_catalog().as_ref())
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
            let def = defs.iter().find(|def| &def.name == tool);
            let shaped_by_caller = def.is_some_and(|def| {
                ["output_schema", "shape"]
                    .iter()
                    .any(|key| def.input_schema["properties"].get(key).is_some())
            });
            if shaped_by_caller {
                continue;
            }
            let schema = def
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
    let mut problems = Vec::new();
    for (index, step) in doc.steps.iter().enumerate() {
        if matches!(step.tool_name.as_str(), "exit" | "ask") {
            continue;
        }
        let readers = [
            json!(&doc.steps[index + 1..]),
            json!(doc.output),
            json!(doc.solver),
        ];
        if !readers.iter().any(|value| reads_root(value, &step.id)) {
            problems.push(format!(
                "step {}: its result is never used by a later step or the output; remove it or use it",
                step.id
            ));
        }
    }
    problems
}

fn reads_root(value: &Value, root: &str) -> bool {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) if text.contains("{{") => {
                let roots = crate::template::referenced_roots(text).unwrap_or_default();
                if roots.iter().any(|found| found == root) {
                    return true;
                }
            }
            Value::Object(map) => stack.extend(map.values()),
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    false
}

fn snake_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
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
                        .filter(|key| {
                            !given.is_some_and(|given| {
                                given.contains_key(*key) || given.contains_key(&snake_case(key))
                            })
                        })
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

fn split_key(yaml: &str, key: &str) -> (String, Vec<Value>) {
    let Ok(mut doc) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return (yaml.to_string(), Vec::new());
    };
    let Some(questions) = doc.as_mapping_mut().and_then(|map| map.remove(key)) else {
        return (yaml.to_string(), Vec::new());
    };
    let questions = serde_json::to_value(questions)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let yaml = serde_yaml::to_string(&doc).unwrap_or_else(|_| yaml.to_string());
    (yaml, questions)
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
    let replacements: Vec<serde_yaml::Value> = match patch.get("steps") {
        Some(serde_yaml::Value::Sequence(list)) => list.clone(),
        Some(serde_yaml::Value::Mapping(by_id)) => by_id
            .iter()
            .map(|(id, step)| {
                let mut step = step.clone();
                if let Some(map) = step.as_mapping_mut() {
                    if !map.contains_key("id") {
                        map.insert("id".into(), id.clone());
                    }
                }
                step
            })
            .collect(),
        _ => Vec::new(),
    };
    for replacement in replacements {
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
    fn a_required_input_may_use_the_file_spelling_of_its_name() {
        let defs = vec![ToolDef {
            name: "ask".to_string(),
            description: String::new(),
            input_schema: json!({"type": "object", "required": ["prompt", "outputSchema"]}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }];
        let steps = json!([
            {"id": "a", "tool_name": "ask", "input": {"prompt": "x", "output_schema": {}}},
            {"id": "b", "tool_name": "ask", "input": {"prompt": "x", "outputSchema": {}}},
            {"id": "c", "tool_name": "ask", "input": {"prompt": "x"}},
        ]);
        assert_eq!(
            missing_inputs(&steps, &defs),
            ["step c: ask requires outputSchema"]
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
    fn a_step_read_through_a_section_or_a_padded_tag_is_used() {
        let yaml = "version: 2\nidentifier: p\nname: p\ndescription: d\nsteps:\n  - id: commits\n    tool_name: t__search\n    input: {query: x}\n  - id: other\n    tool_name: t__search\n    input: {query: y}\n  - id: summary\n    tool_name: t__issues\n    input: {teamId: \"{{#commits.values}}{{title}}{{/commits.values}} {{ other.count }}\"}\noutput: {issues: \"{{summary}}\"}\n";
        let doc = parse_plan_source(yaml, "plan").unwrap();
        assert!(unused_steps(&doc).is_empty(), "{:?}", unused_steps(&doc));
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
    fn a_tool_whose_output_each_call_defines_is_not_held_to_a_recorded_shape() {
        let yaml = "version: 2\nidentifier: p\nname: p\ndescription: d\nsteps:\n  - id: summary\n    tool_name: builtin__infer\n    input: {instruction: x}\noutput: {a: \"{{summary.text}}\"}\n";
        let doc = parse_plan_source(yaml, "plan").unwrap();
        let defs = vec![ToolDef {
            name: "builtin__infer".to_string(),
            description: String::new(),
            input_schema: json!({"type": "object", "properties": {"instruction": {}, "output_schema": {}}}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }];
        let shapes = std::collections::HashMap::from([(
            "builtin__infer".to_string(),
            crate::store::ToolShape {
                tool: "builtin__infer".to_string(),
                schema: json!({"type": "object", "properties": {"patch": {}}}),
                example: json!({"patch": ""}),
                seen_count: 1,
            },
        )]);
        assert!(unknown_fields(&doc, &defs, &shapes).is_empty());
    }

    #[test]
    fn a_draft_s_questions_become_one_form_with_an_open_instructions_field() {
        let form = question_form(json!({"draft": {"yaml": "x", "questions": [
            {"key": "status", "question": "Which status means in progress?", "guess": "Started", "options": ["Backlog", "Started"]},
            {"key": "project", "question": "Which project?", "guess": "Mobile"},
            {"key": "label", "question": "Which label?", "guess": "bug", "options": ["feature"]},
            {"key": "status", "question": "Asked twice"},
            {"question": "No key"}
        ]}}))
        .unwrap();
        assert_eq!(form["count"], 3);
        assert_eq!(
            form["schema"]["propertyOrder"],
            json!(["status", "project", "label", "instructions"])
        );
        assert_eq!(
            form["schema"]["properties"]["status"],
            json!({"type": "string", "description": "Which status means in progress?", "enum": ["Backlog", "Started"]})
        );
        assert_eq!(
            form["schema"]["properties"]["instructions"]["description"],
            "Anything else the planner should know?"
        );
        assert_eq!(
            form["default"],
            json!({"status": "Started", "project": "Mobile"}),
            "a guess outside the options is not offered"
        );
        assert!(super::super::ask::answer_schema_problem(&form["schema"]).is_none());
    }

    #[test]
    fn questions_written_inside_the_plan_are_asked_and_left_out_of_it() {
        let yaml = "version: 2\nidentifier: p\nquestions:\n  - {key: state, question: Which state?, guess: started}\nsteps: []\n";
        let form = question_form(json!({"draft": {"yaml": yaml}})).unwrap();
        assert_eq!(form["count"], 1);
        assert_eq!(form["default"], json!({"state": "started"}));
        let (plan, questions) = split_key(yaml, "questions");
        assert_eq!(questions.len(), 1);
        assert!(!plan.contains("questions"), "{plan}");
    }

    #[test]
    fn a_draft_without_questions_asks_nothing() {
        for draft in [json!({"yaml": "x"}), json!({"yaml": "x", "questions": []})] {
            let form = question_form(json!({ "draft": draft })).unwrap();
            assert_eq!(form["count"], 0);
            assert_eq!(form["schema"]["properties"], json!({}));
        }
    }

    #[test]
    fn a_search_written_inside_the_plan_is_a_request_not_a_plan_key() {
        let yaml = "version: 2\nidentifier: p\nsearch: [List workflow states]\nsteps: []\n";
        let (plan, search) = split_key(yaml, "search");
        assert_eq!(search, vec![json!("List workflow states")]);
        assert!(!plan.contains("search"), "{plan}");
    }

    #[test]
    fn a_patch_may_key_its_steps_by_id() {
        let patch = "steps:\n  b:\n    tool_name: t__issues\n    input: {teamId: y}\n";
        let merged = apply_patch(PLAN, patch).unwrap();
        let doc = parse_plan_source(&merged, "plan").unwrap();
        assert_eq!(doc.steps.len(), 2);
        assert_eq!(doc.steps[1].id, "b");
        assert_eq!(doc.steps[1].input["teamId"], "y");
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
