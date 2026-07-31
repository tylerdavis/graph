//! The tool catalog graph serves over MCP.
//!
//! Two populations, deliberately named apart:
//!
//! - **Authoring tools** (`graph_plan_*`, `graph_tools_*`) are static. They
//!   are the plan-authoring CLI, one tool per verb, so an agent can build and
//!   check a plan without a shell.
//! - **Execution tools** (`plan_<identifier>`) are generated from the plan
//!   catalog on this machine, one per plan, carrying that plan's own
//!   `input_schema`. This is the half that makes a plan a capability inside
//!   somebody else's agent.
//!
//! The `graph_` prefix on the authoring half is what keeps a plan named
//! `tools_list` from colliding with the verb of the same name.

use graph_core::pipeline::doc::PlanDoc;
use rmcp::model::Tool;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::sync::Arc;

/// Prefix for the per-plan execution tools.
pub const PLAN_PREFIX: &str = "plan_";

fn object(schema: Value) -> Arc<Map<String, Value>> {
    Arc::new(match schema {
        Value::Object(map) => map,
        _ => Map::new(),
    })
}

fn tool(name: &'static str, description: &'static str, schema: Value) -> Tool {
    Tool::new(
        Cow::Borrowed(name),
        Cow::Borrowed(description),
        object(schema),
    )
}

/// A plan target: an identifier in the catalog, or a path to a YAML file.
fn target_property() -> Value {
    json!({
        "type": "string",
        "description": "A plan identifier (as in `plan list`) or a path to a plan YAML file."
    })
}

/// The static half of the catalog.
pub fn authoring_tools() -> Vec<Tool> {
    vec![
        tool(
            "graph_plan_list",
            "List the plans available on this machine, plus any plan files that failed to \
             load (`skipped`) and any hidden because they need an MCP server this machine \
             does not configure (`hidden`). Start here to find out what already exists.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "graph_plan_show",
            "Read one plan's full definition: its steps, input schema, and finish mode. Use \
             this before editing a plan, and to inspect a plan you want to model a new one on.",
            json!({
                "type": "object",
                "required": ["target"],
                "properties": {"target": target_property()}
            }),
        ),
        tool(
            "graph_plan_validate",
            "Check a plan against every layer: structure, template references, and whether \
             its tools resolve on this machine. Returns `problems` (fatal, makes `ok` false) \
             separately from `notes` (portable but not runnable here). This is the verdict — \
             do not judge a plan by reading it.",
            json!({
                "type": "object",
                "required": ["target"],
                "properties": {"target": target_property()}
            }),
        ),
        tool(
            "graph_plan_new",
            "Scaffold an empty plan file. It is deliberately invalid until it has steps, so \
             `ok` is true while `problems` still lists 'plan has no steps' — the two are \
             different questions. Use this when there are no model credentials, or when you \
             already know every step; otherwise prefer graph_plan_draft.",
            json!({
                "type": "object",
                "required": ["identifier"],
                "properties": {
                    "identifier": {"type": "string", "description": "Tool-name-safe id, also the file name: [a-zA-Z0-9_-]."},
                    "name": {"type": "string", "description": "Display name. Defaults to the identifier."},
                    "description": {"type": "string", "description": "What the plan does — this is the routing signal an agent selects on."}
                }
            }),
        ),
        tool(
            "graph_plan_draft",
            "Draft a plan from a goal using the planner model, grounded in this machine's \
             real tool catalog. COSTS INFERENCE and takes ~30s. Every step is validated as \
             it is generated, so a returned draft is always statically valid. \
             Drafting happens ONCE, at the start: it replaces every step, so never call it \
             to correct a plan that already has steps worth keeping — use the edit tools, \
             which apply one change each and are refused if they would break the plan. \
             Pass `from` to draft into an existing plan's identity and get a good identifier \
             instead of one truncated from the goal.",
            json!({
                "type": "object",
                "required": ["goal"],
                "properties": {
                    "goal": {"type": "string", "description": "What the plan should do, as one self-contained instruction."},
                    "from": {"type": "string", "description": "An existing plan whose identifier, name, description and input schema the draft should adopt. Its steps ARE replaced."}
                }
            }),
        ),
        tool(
            "graph_plan_set",
            "Set one plan-level attribute. Validated atomically: the write is refused, with \
             the problems it would introduce, rather than leaving the plan broken. Changing \
             `identifier` writes a NEW file and reports the leftover as `renamedFrom`.",
            json!({
                "type": "object",
                "required": ["target", "attribute", "value"],
                "properties": {
                    "target": target_property(),
                    "attribute": {
                        "type": "string",
                        "enum": ["name", "description", "identifier", "exemplars", "requires_servers", "input_schema", "solver", "output"],
                        "description": "Named exactly as it appears in the plan YAML."
                    },
                    "value": {
                        "description": "A string for scalar attributes; an array of strings for `exemplars` and `requires_servers`; a JSON object for `input_schema`, `solver`, and `output`."
                    }
                }
            }),
        ),
        tool(
            "graph_plan_unset",
            "Clear one optional plan attribute. `name`, `description`, and `identifier` are \
             required and cannot be cleared.",
            json!({
                "type": "object",
                "required": ["target", "attribute"],
                "properties": {
                    "target": target_property(),
                    "attribute": {
                        "type": "string",
                        "enum": ["exemplars", "requires_servers", "input_schema", "solver", "output"]
                    }
                }
            }),
        ),
        tool(
            "graph_plan_step_add",
            "Add a step to a plan, appended or anchored before/after an existing step id. \
             Validated atomically — a step referencing something that is not `input` or an \
             earlier step is refused.",
            json!({
                "type": "object",
                "required": ["target", "id", "tool", "input"],
                "properties": {
                    "target": target_property(),
                    "id": {"type": "string", "description": "Step id — how later steps reference it as {{<id>.field}}."},
                    "tool": {"type": "string", "description": "A tool name from graph_tools_list, or a control step: exit, agent, decide, map, reduce."},
                    "input": {"type": "object", "description": "The step's input object. Leaf strings may be templates over earlier results, e.g. {{E1.field}} or {{input.x}}."},
                    "reasoning": {"type": "string", "description": "Why this step exists, carried into the plan for readers."},
                    "before": {"type": "string", "description": "Insert before this step id instead of appending."},
                    "after": {"type": "string", "description": "Insert after this step id instead of appending."}
                }
            }),
        ),
        tool(
            "graph_plan_step_update",
            "Change one attribute of one step. To rename a step use graph_plan_step_rename, \
             which also rewrites every downstream reference.",
            json!({
                "type": "object",
                "required": ["target", "id", "attribute", "value"],
                "properties": {
                    "target": target_property(),
                    "id": {"type": "string"},
                    "attribute": {"type": "string", "enum": ["tool", "input", "reasoning"]},
                    "value": {"description": "A string for `tool` and `reasoning`; a JSON object for `input`."}
                }
            }),
        ),
        tool(
            "graph_plan_step_rename",
            "Rename a step, rewriting every downstream {{id.…}} reference to match.",
            json!({
                "type": "object",
                "required": ["target", "id", "new_id"],
                "properties": {
                    "target": target_property(),
                    "id": {"type": "string"},
                    "new_id": {"type": "string"}
                }
            }),
        ),
        tool(
            "graph_plan_step_rm",
            "Remove a step. Refused if a later step still references it.",
            json!({
                "type": "object",
                "required": ["target", "id"],
                "properties": {"target": target_property(), "id": {"type": "string"}}
            }),
        ),
        tool(
            "graph_tools_list",
            "List every tool a plan step may call on this machine, with the namespaced name \
             (`server__tool`, `user__tool`, `builtin__tool`, `plan__id`) that a step's \
             `tool` field must use. Schemas are omitted — use graph_tools_show for one tool.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "graph_tools_show",
            "Show one tool's description and schemas. `inputSchema` is what a plan step's \
             input object must satisfy. Every key is always present, null when absent.",
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {"name": {"type": "string", "description": "The namespaced tool name."}}
            }),
        ),
        tool(
            "graph_tools_test",
            "Invoke one tool directly and return what it produced. Use this to learn a \
             tool's real output shape before writing a template path against it, instead of \
             guessing at field names. A tool that reports failure comes back with \
             `isError: true` rather than as a call failure.",
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {"type": "string"},
                    "input": {"type": "object", "description": "The tool's input object."}
                }
            }),
        ),
    ]
}

/// A plan, as a callable MCP tool.
///
/// The description is the plan's own description plus its exemplars, because
/// that is exactly what graph itself routes on when an agent picks a plan —
/// the calling agent deserves the same signal.
pub fn plan_tool(doc: &PlanDoc) -> Tool {
    let mut description = doc.description.clone();
    if !doc.exemplars.is_empty() {
        description.push_str("\n\nHandles requests like:");
        for exemplar in &doc.exemplars {
            description.push_str(&format!("\n- {exemplar}"));
        }
    }
    let schema = doc
        .input_schema
        .clone()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    Tool::new(
        Cow::Owned(format!("{PLAN_PREFIX}{}", doc.identifier)),
        Cow::Owned(description),
        object(schema),
    )
    .with_title(doc.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> PlanDoc {
        serde_yaml::from_str(
            r#"
identifier: sprint_report
name: Sprint Report
description: Summarize the current sprint.
exemplars: ["how is the sprint going", "sprint status"]
input_schema:
  type: object
  required: [team]
  properties:
    team: { type: string }
steps:
  - id: E1
    tool_name: t__search
    input: { query: x }
"#,
        )
        .unwrap()
    }

    #[test]
    fn a_plan_becomes_a_tool_carrying_its_own_input_schema() {
        let tool = plan_tool(&doc());
        assert_eq!(tool.name, "plan_sprint_report");
        // The plan's schema is the tool's schema: a calling agent gets the
        // same argument checking graph applies internally.
        assert_eq!(tool.input_schema["required"], json!(["team"]));
        assert_eq!(tool.title.as_deref(), Some("Sprint Report"));
    }

    #[test]
    fn exemplars_are_folded_into_the_routing_signal() {
        let tool = plan_tool(&doc());
        let description = tool.description.unwrap();
        assert!(description.contains("Summarize the current sprint."));
        // Exemplars are how graph itself routes to a plan; an agent calling
        // over MCP is making the same decision and needs the same evidence.
        assert!(
            description.contains("how is the sprint going"),
            "{description}"
        );
    }

    #[test]
    fn a_plan_without_an_input_schema_still_gets_a_valid_object_schema() {
        let mut doc = doc();
        doc.input_schema = None;
        let tool = plan_tool(&doc);
        assert_eq!(tool.input_schema["type"], json!("object"));
    }

    #[test]
    fn authoring_tools_are_namespaced_away_from_plan_tools() {
        // A plan named `tools_list` would otherwise collide with the verb.
        for tool in authoring_tools() {
            assert!(tool.name.starts_with("graph_"), "{}", tool.name);
            assert!(
                tool.input_schema.contains_key("type"),
                "{} needs an object schema",
                tool.name
            );
        }
    }
}
