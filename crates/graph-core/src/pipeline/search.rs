use super::catalog::glob_matches;
use super::{prompts, Pipeline};
use crate::tools::ToolDef;
use crate::usage::CallSite;
use graph_config::ModelKind;
use graph_llm::decision::{Answer, DecisionRequest, Question};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const CATALOG_TOOLS_TOOL: &str = "builtin__catalog_tools";

pub const SCORE_CANDIDATES_TOOL: &str = "builtin__score_candidates";

pub const DESCRIBE_TOOLS_TOOL: &str = "builtin__describe_tools";

pub const DEFAULT_DECISION_ROLE: &str = "decider";

pub fn search_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: CATALOG_TOOLS_TOOL.to_string(),
            description: "Lists every tool in the catalog for searching, with its description: \
                          MCP, pack and user tools, plans and agents. Tools on the \
                          always-loaded list are left out and returned separately."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: Some(json!({
                "type": "object",
                "required": ["tools", "always"],
                "properties": {
                    "tools": {"type": "array", "items": {"type": "object"}},
                    "always": {"type": "array", "items": {"type": "string"}}
                }
            })),
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: SCORE_CANDIDATES_TOOL.to_string(),
            description: "Scores candidates (each a `name` and `description`) for a task, from \
                          0 to 1. With a decision model configured, the candidates are the \
                          options of one choice question (`question`, then the task), whose \
                          probabilities are the scores; more than `chunk_size` candidates are \
                          split into choices sent together, and every candidate reaching \
                          `threshold` in its own choice meets the others in a final choice. Otherwise, or if that call fails, candidates are ranked \
                          by keyword overlap. Returns every candidate highest score first, and \
                          `selected`: those at or above `threshold`, topped up to `floor` with \
                          the best of the rest, capped at `limit`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query", "candidates", "question"],
                "properties": {
                    "query": {"type": "string", "description": "The task the candidates are scored against"},
                    "candidates": {"type": "array", "items": {"type": "object"}, "description": "Objects with `name` and `description`"},
                    "question": {"type": "string", "description": "The choice question; the task follows it"},
                    "threshold": {"type": "number", "description": "Minimum score to be selected; default 0.5"},
                    "floor": {"type": "integer", "description": "Fewest candidates to select, taking the best below the threshold when needed; default 0"},
                    "limit": {"type": "integer", "description": "Most candidates to select"},
                    "model": {"type": "string", "description": "The decision model role; default decider"},
                    "chunk_size": {"type": "integer", "description": "Most options per choice question, up to 255; larger sets are split into choices sent together, then a final choice among every candidate that reached the threshold. Default 250"}
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
    250
}

const MAX_CHOICE_OPTIONS: usize = 255;

const NO_TOOL_OPTION: &str = "no_tool";

const NO_TOOL_DESCRIPTION: &str = "No tool in this list fits the task.";

struct Choice {
    scores: Vec<f64>,
    no_fit: bool,
}

fn balanced_chunks(indexes: &[usize], size: usize) -> Vec<Vec<usize>> {
    let count = indexes.len().div_ceil(size);
    let len = indexes.len().div_ceil(count);
    indexes.chunks(len).map(<[usize]>::to_vec).collect()
}

#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    text: String,
    description: String,
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

const AUTHORING_PLANS: &[&str] = &["search_tools", "compose_plan"];

impl Pipeline {
    pub(super) async fn searchable_tools(&self) -> Vec<ToolDef> {
        let mut tools = self.registry.tools().await.unwrap_or_default();
        tools.extend(self.callable_plan_defs().into_iter().filter(|tool| {
            !AUTHORING_PLANS
                .iter()
                .any(|plan| tool.name == format!("plan__{plan}"))
        }));
        tools.extend(self.agent_tool_defs());
        tools
    }

    pub(super) async fn catalog_tools(&self) -> Result<Value, String> {
        let mut tools = Vec::new();
        let mut always = Vec::new();
        for tool in self.searchable_tools().await {
            if is_always_loaded(&self.always_loaded, &tool.name) {
                always.push(tool.name);
            } else {
                tools.push(json!({"name": tool.name, "description": tool.description}));
            }
        }
        always.sort();
        Ok(json!({ "tools": tools, "always": always }))
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
            Some(Choice {
                scores: Vec::new(),
                no_fit: true,
            })
        } else if self.router.kind_of_role(&role) == Some(ModelKind::Decision) {
            match self.decision_scores(&role, &input, &candidates).await {
                Ok(choice) => Some(choice),
                Err(error) => {
                    fallback_reason = Some(error);
                    None
                }
            }
        } else {
            None
        };
        let (mode, scores, no_fit) = match decided {
            Some(choice) => ("decision", choice.scores, choice.no_fit),
            None => ("keyword", keyword_scores(&input.query, &candidates), false),
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
        if no_fit {
            take = 0;
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
        let mut out =
            json!({ "mode": mode, "scored": entries, "selected": selected, "no_fit": no_fit });
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
    ) -> Result<Choice, String> {
        let instructions = format!("{}\n\n{}", input.question.trim(), input.query.trim());
        let size = input.chunk_size.clamp(2, MAX_CHOICE_OPTIONS - 1);
        let indexes: Vec<usize> = (0..candidates.len()).collect();
        if candidates.len() <= size {
            return self.choose(role, &instructions, candidates, &indexes).await;
        }
        let chunks = balanced_chunks(&indexes, size);
        let rounds = chunks
            .iter()
            .map(|chunk| self.choose(role, &instructions, candidates, chunk));
        let mut first_round: Vec<(usize, f64)> = Vec::new();
        for (chunk, round) in chunks.iter().zip(futures::future::join_all(rounds).await) {
            first_round.extend(chunk.iter().copied().zip(round?.scores));
        }
        let finalists: Vec<usize> = first_round
            .iter()
            .filter(|(_, probability)| *probability >= input.threshold)
            .map(|(i, _)| *i)
            .collect();
        let mut scores = vec![0.0; candidates.len()];
        if finalists.is_empty() {
            for (index, probability) in first_round {
                scores[index] = probability;
            }
            return Ok(Choice {
                scores,
                no_fit: true,
            });
        }
        let final_round = self
            .choose(role, &instructions, candidates, &finalists)
            .await?;
        for (index, probability) in finalists.into_iter().zip(final_round.scores) {
            scores[index] = probability;
        }
        Ok(Choice {
            scores,
            no_fit: final_round.no_fit,
        })
    }

    async fn choose(
        &self,
        role: &str,
        instructions: &str,
        candidates: &[Candidate],
        indexes: &[usize],
    ) -> Result<Choice, String> {
        let mut criteria: BTreeMap<String, String> = indexes
            .iter()
            .map(|&i| {
                (
                    candidates[i].name.clone(),
                    prompts::summary_line(&candidates[i].description),
                )
            })
            .collect();
        criteria.insert(NO_TOOL_OPTION.to_string(), NO_TOOL_DESCRIPTION.to_string());
        let request = DecisionRequest {
            model: String::new(),
            state: Value::Null,
            questions: BTreeMap::from([(
                "tool".to_string(),
                Question::Choice {
                    instructions: instructions.to_string(),
                    criteria,
                },
            )]),
        };
        let response = CallSite::as_role(role, self.router.decide_named(Some(role), request))
            .await
            .map_err(|e| format!("decision model '{role}' failed: {e}"))?;
        let Some(Answer::Choice { probabilities, .. }) = response.answers.get("tool") else {
            return Err(format!("decision model '{role}' did not answer the choice"));
        };
        let scores: Vec<f64> = indexes
            .iter()
            .map(|&i| {
                probabilities
                    .get(&candidates[i].name)
                    .copied()
                    .unwrap_or(0.0)
            })
            .collect();
        let none = probabilities.get(NO_TOOL_OPTION).copied().unwrap_or(0.0);
        let best = scores.iter().copied().fold(0.0, f64::max);
        Ok(Choice {
            no_fit: none > best,
            scores,
        })
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
}
