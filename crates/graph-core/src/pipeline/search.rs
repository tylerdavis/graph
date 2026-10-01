use super::catalog::glob_matches;
use super::{prompts, Pipeline};
use crate::tools::ToolDef;
use crate::usage::CallSite;
use crate::user_tools::{pack_of, pack_summary};
use graph_config::ModelKind;
use graph_llm::decision::{Answer, DecisionRequest, Question};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const TOOL_GROUPS_TOOL: &str = "builtin__tool_groups";

pub const SCORE_CANDIDATES_TOOL: &str = "builtin__score_candidates";

pub const GROUP_TOOLS_TOOL: &str = "builtin__group_tools";

pub const DESCRIBE_TOOLS_TOOL: &str = "builtin__describe_tools";

pub const DEFAULT_DECISION_ROLE: &str = "decider";

pub fn search_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TOOL_GROUPS_TOOL.to_string(),
            description: "Lists the catalog's tool groups for searching: each MCP server, each \
                          tool pack, user tools, plans and agents, with a description and the \
                          tools in it. Tools on the always-loaded list are left out and \
                          returned separately."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: Some(json!({
                "type": "object",
                "required": ["groups", "always"],
                "properties": {
                    "groups": {"type": "array", "items": {"type": "object"}},
                    "always": {"type": "array", "items": {"type": "string"}}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: SCORE_CANDIDATES_TOOL.to_string(),
            description: "Scores candidates (each a `name` and `description`) for how well they \
                          fit a task, independently, from 0 to 1. With a decision model \
                          configured, each candidate is one likelihood question built from \
                          `question` ({name} and {description} are filled in), sent in \
                          requests of at most `chunk_size` candidates; otherwise, or \
                          if that call fails, candidates are ranked by keyword overlap. Returns \
                          every candidate highest score first, and `selected`: those at or \
                          above `threshold`, topped up to `floor` with the best of the rest, \
                          capped at `limit`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query", "candidates", "question"],
                "properties": {
                    "query": {"type": "string", "description": "The task the candidates are scored against"},
                    "candidates": {"type": "array", "items": {"type": "object"}, "description": "Objects with `name` and `description`"},
                    "question": {"type": "string", "description": "The likelihood question per candidate, with {name} and {description} placeholders"},
                    "threshold": {"type": "number", "description": "Minimum score to be selected; default 0.5"},
                    "floor": {"type": "integer", "description": "Fewest candidates to select, taking the best below the threshold when needed; default 0"},
                    "limit": {"type": "integer", "description": "Most candidates to select"},
                    "model": {"type": "string", "description": "The decision model role; default decider"},
                    "chunk_size": {"type": "integer", "description": "Most candidates per decision request; larger sets are split into requests sent together. Default 60"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["mode", "scored", "selected"],
                "properties": {
                    "mode": {"type": "string", "enum": ["decision", "keyword"]},
                    "scored": {"type": "array", "items": {"type": "object"}},
                    "selected": {"type": "array", "items": {"type": "object"}},
                    "fallback_reason": {"type": "string"}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: GROUP_TOOLS_TOOL.to_string(),
            description: "Lists the tools in the given groups (names, or the `selected` \
                          entries builtin__score_candidates returned), each with its group and \
                          description, ready to score."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["groups"],
                "properties": {
                    "groups": {"type": "array", "description": "Group names, or objects with a `name`"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["tools"],
                "properties": {"tools": {"type": "array", "items": {"type": "object"}}}
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: DESCRIBE_TOOLS_TOOL.to_string(),
            description: "Describes tools in the order given: name, description and score, and \
                          with `schemas` (the default) the input schema and the declared or \
                          observed output shape."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["tools"],
                "properties": {
                    "tools": {"type": "array", "description": "Tool names, or objects with a `name` and optional `score`"},
                    "schemas": {"type": "boolean", "description": "Include schemas and output shapes; default true"}
                }
            }),
            output_schema: Some(json!({
                "type": "object",
                "required": ["tools"],
                "properties": {"tools": {"type": "array", "items": {"type": "object"}}}
            })),
            output_example: None,
            read_only: Some(true),
        },
    ]
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Group {
    pub name: String,
    pub kind: &'static str,
    pub description: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ScoreInput {
    query: String,
    candidates: Vec<Value>,
    question: String,
    #[serde(default = "default_threshold")]
    threshold: f64,
    #[serde(default)]
    floor: usize,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "default_chunk_size")]
    chunk_size: usize,
}

fn default_threshold() -> f64 {
    0.5
}

fn default_chunk_size() -> usize {
    60
}

#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    text: String,
    description: String,
}

pub(super) fn group_of(tool: &str) -> (String, &'static str) {
    if tool.starts_with("plan__") {
        return ("plans".to_string(), "plans");
    }
    if tool.starts_with("agent__") {
        return ("agents".to_string(), "agents");
    }
    if tool.starts_with("user__") {
        return ("user".to_string(), "user");
    }
    if let Some(bare) = tool.strip_prefix("builtin__") {
        return (pack_of(bare).unwrap_or("builtin").to_string(), "pack");
    }
    match tool.split_once("__") {
        Some((server, _)) => (server.to_string(), "mcp"),
        None => (tool.to_string(), "mcp"),
    }
}

pub(super) fn is_always_loaded(patterns: &[String], tool: &str) -> bool {
    patterns.iter().any(|pattern| glob_matches(pattern, tool))
}

pub(super) fn names_of(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(name) => Some(name.clone()),
                    Value::Object(map) => {
                        map.get("name").and_then(Value::as_str).map(str::to_string)
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Pipeline {
    pub(super) async fn searchable_tools(&self) -> Vec<ToolDef> {
        let mut tools = self.registry.tools().await.unwrap_or_default();
        tools.extend(self.callable_plan_defs());
        tools.extend(self.agent_tool_defs());
        tools
    }

    pub(super) async fn tool_groups(&self) -> (Vec<Group>, Vec<String>) {
        let servers: HashMap<String, Option<String>> = self
            .registry
            .servers()
            .await
            .into_iter()
            .map(|server| (server.name, server.description))
            .collect();
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        let mut always = Vec::new();
        for tool in self.searchable_tools().await {
            if is_always_loaded(&self.always_loaded, &tool.name) {
                always.push(tool.name);
                continue;
            }
            let (name, kind) = group_of(&tool.name);
            groups
                .entry(name.clone())
                .or_insert_with(|| Group {
                    name: name.clone(),
                    kind,
                    description: String::new(),
                    tools: Vec::new(),
                })
                .tools
                .push(tool.name);
        }
        for group in groups.values_mut() {
            let described = match group.kind {
                "mcp" => servers.get(&group.name).cloned().flatten(),
                "pack" => pack_summary(&group.name).map(str::to_string),
                "user" => Some("Tools defined in this project or your config.".to_string()),
                "plans" => Some("Saved plans that can run as a single step.".to_string()),
                "agents" => Some("Named agents that take a task and return a result.".to_string()),
                _ => None,
            };
            group.description = match described {
                Some(text) if !text.trim().is_empty() => {
                    format!("{} Tools: {}.", text.trim(), group.tools.join(", "))
                }
                _ => format!("Tools: {}.", group.tools.join(", ")),
            };
        }
        always.sort();
        (groups.into_values().collect(), always)
    }

    pub(super) async fn tool_groups_value(&self) -> Result<Value, String> {
        let (groups, always) = self.tool_groups().await;
        Ok(json!({
            "groups": groups
                .iter()
                .map(|group| json!({
                    "name": group.name,
                    "kind": group.kind,
                    "description": group.description,
                    "tools": group.tools,
                }))
                .collect::<Vec<_>>(),
            "always": always,
        }))
    }

    pub(super) async fn group_tools(&self, input: Value) -> Result<Value, String> {
        let wanted: BTreeSet<String> = names_of(&input["groups"]).into_iter().collect();
        let tools: Vec<Value> = self
            .searchable_tools()
            .await
            .into_iter()
            .filter(|tool| !is_always_loaded(&self.always_loaded, &tool.name))
            .filter_map(|tool| {
                let (group, _) = group_of(&tool.name);
                wanted.contains(&group).then(
                    || json!({"name": tool.name, "group": group, "description": tool.description}),
                )
            })
            .collect();
        Ok(json!({ "tools": tools }))
    }

    pub(super) async fn describe_tools(&self, input: Value) -> Result<Value, String> {
        let schemas = input["schemas"].as_bool().unwrap_or(true);
        let wanted = input["tools"].as_array().cloned().unwrap_or_default();
        let defs: HashMap<String, ToolDef> = self
            .searchable_tools()
            .await
            .into_iter()
            .map(|tool| (tool.name.clone(), tool))
            .collect();
        let shapes = if schemas {
            self.shapes().await
        } else {
            HashMap::new()
        };
        let mut tools = Vec::new();
        for item in wanted {
            let (name, extra) = match &item {
                Value::String(name) => (name.clone(), serde_json::Map::new()),
                Value::Object(map) => match map.get("name").and_then(Value::as_str) {
                    Some(name) => (name.to_string(), map.clone()),
                    None => continue,
                },
                _ => continue,
            };
            let Some(def) = defs.get(&name) else {
                continue;
            };
            let mut entry = if schemas {
                let text = prompts::describe_tools(std::slice::from_ref(def), &shapes);
                serde_json::from_str::<Value>(text.trim()).unwrap_or_else(|_| json!({}))
            } else {
                json!({"name": def.name, "description": def.description})
            };
            for key in ["score", "below_threshold"] {
                if let Some(value) = extra.get(key) {
                    entry[key] = value.clone();
                }
            }
            tools.push(entry);
        }
        Ok(json!({ "tools": tools }))
    }

    pub(super) async fn score_candidates(&self, input: Value) -> Result<Value, String> {
        let input: ScoreInput = serde_json::from_value(input)
            .map_err(|e| format!("invalid score_candidates input: {e}"))?;
        if !(0.0..=1.0).contains(&input.threshold) {
            return Err("`threshold` must be between 0 and 1".to_string());
        }
        let candidates: Vec<Candidate> = input
            .candidates
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_string();
                let description = item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let members = names_of(item.get("tools").unwrap_or(&Value::Null)).join(" ");
                Some(Candidate {
                    text: format!("{name} {description} {members}"),
                    name,
                    description,
                })
            })
            .collect();
        let role = input
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_DECISION_ROLE.to_string());
        let mut fallback_reason = None;
        let decided = if candidates.is_empty() {
            Some(Vec::new())
        } else if self.router.kind_of_role(&role) == Some(ModelKind::Decision) {
            match self.decision_scores(&role, &input, &candidates).await {
                Ok(scores) => Some(scores),
                Err(error) => {
                    fallback_reason = Some(error);
                    None
                }
            }
        } else {
            None
        };
        let (mode, scores) = match decided {
            Some(scores) => ("decision", scores),
            None => ("keyword", keyword_scores(&input.query, &candidates)),
        };
        let mut scored: Vec<(Candidate, f64)> = candidates.into_iter().zip(scores).collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        let above = scored
            .iter()
            .filter(|(_, score)| *score >= input.threshold)
            .count();
        let mut take = above.max(input.floor.min(scored.len()));
        if let Some(limit) = input.limit {
            take = take.min(limit);
        }
        let entries: Vec<Value> = scored
            .iter()
            .enumerate()
            .map(|(index, (candidate, score))| {
                json!({
                    "name": candidate.name,
                    "description": candidate.description,
                    "score": round(*score),
                    "selected": index < take,
                    "below_threshold": *score < input.threshold,
                })
            })
            .collect();
        let selected: Vec<Value> = entries.iter().take(take).cloned().collect();
        let mut out = json!({ "mode": mode, "scored": entries, "selected": selected });
        if let Some(reason) = fallback_reason {
            out["fallback_reason"] = json!(reason);
        }
        Ok(out)
    }

    async fn decision_scores(
        &self,
        role: &str,
        input: &ScoreInput,
        candidates: &[Candidate],
    ) -> Result<Vec<f64>, String> {
        let size = input.chunk_size.max(1);
        let requests = candidates.chunks(size).map(|chunk| async move {
            let questions: BTreeMap<String, Question> = chunk
                .iter()
                .enumerate()
                .map(|(index, candidate)| {
                    let instructions = input
                        .question
                        .replace("{name}", &candidate.name)
                        .replace("{description}", &candidate.description);
                    (
                        format!("c{index}"),
                        Question::Likelihood {
                            instructions,
                            criteria: None,
                        },
                    )
                })
                .collect();
            let request = DecisionRequest {
                model: String::new(),
                state: json!({ "task": input.query }),
                questions,
            };
            let response = CallSite::as_role(role, self.router.decide_named(Some(role), request))
                .await
                .map_err(|e| format!("decision model '{role}' failed: {e}"))?;
            (0..chunk.len())
                .map(|index| match response.answers.get(&format!("c{index}")) {
                    Some(Answer::Likelihood { probability }) => Ok(*probability),
                    Some(_) => Err(format!(
                        "decision model '{role}' answered c{index} with the wrong kind"
                    )),
                    None => Err(format!("decision model '{role}' did not answer c{index}")),
                })
                .collect::<Result<Vec<f64>, String>>()
        });
        let mut scores = Vec::with_capacity(candidates.len());
        for chunk in futures::future::join_all(requests).await {
            scores.extend(chunk?);
        }
        Ok(scores)
    }

    pub(super) async fn shapes(&self) -> HashMap<String, crate::store::ToolShape> {
        match &self.store {
            Some(store) => store
                .tool_shapes()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|shape| (shape.tool.clone(), shape))
                .collect(),
            None => HashMap::new(),
        }
    }
}

fn round(score: f64) -> f64 {
    (score * 1000.0).round() / 1000.0
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 1)
        .map(str::to_lowercase)
        .collect()
}

fn keyword_scores(query: &str, candidates: &[Candidate]) -> Vec<f64> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let docs: Vec<Vec<String>> = candidates.iter().map(|c| tokenize(&c.text)).collect();
    let terms: BTreeSet<String> = tokenize(query).into_iter().collect();
    if docs.is_empty() || terms.is_empty() {
        return vec![0.0; candidates.len()];
    }
    let n = docs.len() as f64;
    let average = docs.iter().map(Vec::len).sum::<usize>() as f64 / n;
    let raw: Vec<f64> = docs
        .iter()
        .map(|doc| {
            let length = doc.len() as f64;
            terms
                .iter()
                .map(|term| {
                    let frequency = doc.iter().filter(|word| *word == term).count() as f64;
                    if frequency == 0.0 {
                        return 0.0;
                    }
                    let containing = docs.iter().filter(|d| d.contains(term)).count() as f64;
                    let idf = ((n - containing + 0.5) / (containing + 0.5) + 1.0).ln();
                    let norm = if average > 0.0 { length / average } else { 1.0 };
                    idf * frequency * (K1 + 1.0) / (frequency + K1 * (1.0 - B + B * norm))
                })
                .sum()
        })
        .collect();
    let max = raw.iter().cloned().fold(0.0, f64::max);
    if max <= 0.0 {
        return vec![0.0; candidates.len()];
    }
    raw.into_iter().map(|score| score / max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, description: &str) -> Candidate {
        Candidate {
            name: name.to_string(),
            text: format!("{name} {description}"),
            description: description.to_string(),
        }
    }

    #[test]
    fn tools_group_by_namespace_and_pack() {
        assert_eq!(
            group_of("linear__list_issues"),
            ("linear".to_string(), "mcp")
        );
        assert_eq!(
            group_of("builtin__git_diff"),
            ("github".to_string(), "pack")
        );
        assert_eq!(group_of("builtin__infer"), ("llm".to_string(), "pack"));
        assert_eq!(group_of("user__repo_grep"), ("user".to_string(), "user"));
        assert_eq!(group_of("plan__sprint"), ("plans".to_string(), "plans"));
        assert_eq!(group_of("agent__chat"), ("agents".to_string(), "agents"));
    }

    #[test]
    fn always_loaded_patterns_match_names_and_globs() {
        let patterns = vec!["builtin__infer".to_string(), "linear__get_*".to_string()];
        assert!(is_always_loaded(&patterns, "builtin__infer"));
        assert!(is_always_loaded(&patterns, "linear__get_issue"));
        assert!(!is_always_loaded(&patterns, "linear__list_issues"));
    }

    #[test]
    fn keyword_scores_rank_overlap_and_normalize() {
        let candidates = vec![
            candidate("linear", "issue tracking: issues, projects, cycles"),
            candidate("github", "pull requests, diffs and review comments"),
            candidate("slack", "posts messages to channels"),
        ];
        let scores = keyword_scores("review the pull request diff", &candidates);
        assert_eq!(scores[1], 1.0);
        assert_eq!(scores[0], 0.0);
        assert_eq!(scores[2], 0.0);
        assert_eq!(keyword_scores("", &candidates), vec![0.0; 3]);
    }

    #[test]
    fn every_pack_has_a_summary() {
        for pack in crate::user_tools::available_packs() {
            assert!(pack_summary(pack).is_some(), "{pack} has no summary");
        }
    }
}
