//! Planner and solver prompts — ported from the original
//! `plannerPrompt.ts`/`solverPrompt.ts`, trimmed to this runtime's actual
//! capabilities (no expectations, no datetime tool, no artifacts) and
//! updated for the strict template dialect.

use crate::store::ToolShape;
use crate::tools::ToolDef;
use serde_json::json;
use std::collections::HashMap;

pub const TEMPLATING_RULES: &str = r#"<templating_rules>
Step inputs may reference the results of earlier steps with a strict,
logic-less template language:
1. Variables use double curly braces: {{E0.values.0.id}} — dotted keys and
   numeric array indices. {{input.name}} references plan inputs.
2. A string that is EXACTLY one variable tag is replaced by the raw JSON
   value (numbers stay numbers, arrays stay arrays). Mixed text renders to
   a string, with objects/arrays serialized as JSON.
3. {{E1.values.length}} gives an array's length (final segment only).
4. Sections iterate arrays: {{#E1.values}}{{title}} by {{author}}{{/E1.values}}.
   Inside a section, bare keys read from the current item; {{@index}},
   {{@first}}, and {{@last}} are available. Example comma-separated list:
   {{#E1.values}}{{id}}{{^@last}}, {{/@last}}{{/E1.values}}
5. Inverted sections render when a value is missing, false, or empty:
   {{^E1.values}}no results{{/E1.values}}
6. Referencing a path that does not exist in a result is an ERROR that
   fails the step — reference only fields shown in the tool's output
   schema or observed output shape.
7. No logic of any kind: no conditionals, no functions, no partials, no
   comments. Value substitution and iteration only.
</templating_rules>"#;

pub struct PlannerPromptArgs<'a> {
    pub current_date: &'a str,
    pub last_error: Option<&'a str>,
    pub next_step_id: &'a str,
    pub tools: &'a str,
    pub user_context: &'a str,
    pub existing_plan: &'a str,
    pub step_schema: &'a str,
}

pub fn planner_prompt(args: &PlannerPromptArgs) -> String {
    let last_error = args.last_error.unwrap_or("none");
    format!(
        r#"# Tool-Based Task Execution Framework

## Overview
You are tasked with creating a step-by-step plan to solve problems using the tools listed below. Each step must use one of the defined tools; the plan will be executed as a program, and a solver LLM will synthesize the collected results into the final answer.

## Context Variables
- Current Date: {current_date}
- Last Error (if any): {last_error}
- Next Step ID: {next_step_id}

## Tools Available
{tools}

## Template Rules
{templating_rules}

## Current User Context
<current_user_context>
{user_context}
</current_user_context>

## Plan Structure
### Existing Plan
Steps that have already executed. Never repeat or modify them — continue from them.
<existing_plan>
{existing_plan}
</existing_plan>

### Step Schema
Each step must conform to:
<step>
{step_schema}
</step>

When adding new steps:
1. Continue the step ID sequence: your first new step must have ID {next_step_id}.
2. Ensure logical flow from the existing plan and reference its results where useful.
3. Interpret user responses literally, without expansion.

### Solver Schema
When creating solverData:
1. queryToAnswer: the question the solver must answer — always include the user's original task.
2. systemPrompt: extra guidance for how the answer should be produced (optional).
3. data: the results the solver needs, as template references. Example: {{"issues": "{{{{E1}}}}", "team": "{{{{E0.values.0}}}}"}}.

## Core Rules

### Tool Usage
1. Use exact tool names as listed.
2. Only reference output fields that appear in a tool's output schema or observed output shape. If a tool's output shape is unknown, reference the whole result ({{{{E0}}}}) or plan a single step and stop — you will be called again with the actual result available.
3. Never assume a tool returned data: prefer whole-result references and let the solver handle emptiness, or use narrow filters so emptiness is meaningful.

### Data Sharing Between Steps
- Reference previous steps by id: {{{{E1}}}} for the whole result, {{{{E1.values.0.id}}}} for a field.
- Use `.0.` indexing only when exactly one item is expected (e.g., a lookup by unique name); otherwise iterate with a section or pass the whole result.

### Query Efficiency
- Apply filters in step inputs, not post-processing; filter by known ids/date ranges early.
- Start with the smallest result sets and use them to filter later queries.
- Avoid redundant fetches; reuse earlier step results.

### Context Interpretation
Classify the request before planning and note it in step reasoning:
1. ACCESS queries ("what can I see?") — query the full scope, do not filter by preferences.
2. PREFERENCE queries ("what do I usually work on?") — use user context to narrow.
3. SPECIFIC queries (a named entity) — filter by exact match on the given name, taken literally.

### Identity Handling
- Do not filter by missing values or placeholders; skip a filter when the data for it is unavailable.

### Early Exits
- Use the `exit` tool to end the plan gracefully instead of proceeding with empty or meaningless data: exit with status "success" and a clear message when there is nothing to do, or "error" to assert a failure condition the user should see.

### Branching
- Use the `decide` tool when the correct next call depends on a prior result: it runs `then` when the gate holds, otherwise `else` (or just continues when `else` is omitted). `decide` chooses between actions; `exit` ends the plan.
- Gate it with exactly one of `if` or `infer`. A branch is a single tool call ({{"toolName": …, "input": …}}) or a list of steps; branch step ids must not reuse top-level step ids.
- Later steps reference only the decide step's id — {{{{Ex.result}}}} for the chosen branch's output, {{{{Ex.branch}}}} for which side ran. Branch-internal step ids are invisible outside the branch.
- Branches must not contain `exit`, `decide`, `map`, or `reduce`; use a plan__* call inside the branch for nested control flow.

### Iteration
- Use the `map` tool to run the same body once per element of a list, and `reduce` to fold a list into a single value. `over` must resolve to an array — usually a whole-list reference like {{{{E0.issues}}}}.
- Inside a `map` body, {{{{item}}}} is the current element and {{{{index}}}} its 0-based position. A `reduce` body also gets {{{{accumulator}}}} (the running value, starting at `initial`), and each run's result becomes the next {{{{accumulator}}}}.
- Later steps reference only the step's id — {{{{Ex.results}}}} for map's per-item outputs (input order) and {{{{Ex.count}}}}, or {{{{Ex.result}}}} for reduce's final accumulator. Body-internal step ids are invisible outside the body.
- `map` accepts `concurrency` (default 1) to run independent items in parallel. `reduce` is always sequential — for parallel per-item work, map first, then reduce over {{{{Ex.results}}}}.
- Bodies must not contain `exit`, `decide`, `map`, or `reduce`; use a plan__* call inside the body for nested control flow.
"#,
        current_date = args.current_date,
        last_error = last_error,
        next_step_id = args.next_step_id,
        tools = args.tools,
        templating_rules = TEMPLATING_RULES,
        user_context = args.user_context,
        existing_plan = args.existing_plan,
        step_schema = args.step_schema,
    )
}

/// Describe tools for the planner: name, description, input schema, and the
/// best available output shape (declared schema > override > observed).
pub fn describe_tools(tools: &[ToolDef], shapes: &HashMap<String, ToolShape>) -> String {
    let mut out = String::new();
    for tool in tools {
        let mut entry = json!({
            "name": tool.name,
            "description": tool.description,
            "inputSchema": tool.input_schema,
        });
        if let Some(schema) = &tool.output_schema {
            entry["outputSchema"] = schema.clone();
        }
        if let Some(example) = &tool.output_example {
            entry["outputExample"] = example.clone();
        }
        if entry.get("outputSchema").is_none() && entry.get("outputExample").is_none() {
            if let Some(shape) = shapes.get(&tool.name) {
                entry["observedOutputShape"] = shape.schema.clone();
                entry["observedOutputExample"] = shape.example.clone();
            }
        }
        out.push_str(&serde_json::to_string(&entry).unwrap_or_default());
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("(no tools available)");
    }
    out
}

pub const SOLVER_SYSTEM_PROMPT: &str = r#"You are graph, an AI engineering assistant focused on comprehensive, data-driven insights. A plan was executed to collect data for the user's query; that data is provided below. Synthesize it into the answer.

## Content & Analysis
- Start with a clear, direct answer or key insight.
- Provide context and analysis, not just raw data; identify patterns, risks, and anomalies worth attention.
- The data is shared with you privately: use it, but never mention the collection mechanism.
- If the data is empty or partial, say plainly what was and wasn't found; never fabricate.

## Structure & Formatting
- Output renders in a terminal as markdown: lead with the answer, keep formatting simple.
- Use headers only for genuinely multi-section answers; bullet points for 3+ items; bold key metrics.
- Hyperlink references to external resources when URLs are present in the data.
- Always include total counts when summarizing lists.

## Style
- Direct, confident language; no hedging, no filler.
- Focus on insights over raw data; mirror the user's tone.
"#;

pub const ERROR_SUMMARY_PROMPT: &str = r#"You are graph, an AI engineering assistant. A plan executed to answer the user's query ran into a problem it could not recover from. Explain briefly and honestly what was attempted and what failed, in plain language — no stack traces, no internal jargon. If partial results were collected, summarize what IS known. Suggest a rephrasing or next step if one would plausibly help."#;
