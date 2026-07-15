//! Catalog-aware plan validation: the second validation layer.
//!
//! [`super::doc::validate_doc`] and [`Pipeline::validate_plan`] are
//! deliberately static and context-free — they run everywhere a plan is
//! parsed, including places where the runtime catalog isn't available or
//! isn't the same (the workbench). This layer takes the catalog of what is
//! actually loadable as input and resolves every step tool name against
//! it, so a plan that references a nonexistent tool fails at load/validate
//! time instead of mid-run, after earlier steps have already spent tool
//! calls and LLM money.
//!
//! What can and cannot be resolved without side effects:
//! - `builtin__*`, `user__*`, and `plan__*` names resolve exactly — those
//!   catalogs load locally.
//! - MCP `server__tool` names resolve at the *server* level only: listing
//!   a server's tools requires connecting (and spawning stdio children),
//!   which validation must never do. An unconfigured server is a hard
//!   error; the individual tool is still checked at dispatch.
//! - `workbench__*` is rejected by the static layer before this one runs.

use super::body::{parse_branch, Branch};
use super::doc::PlanDoc;
use super::plan::Plan;
use super::{DECIDE_TOOL, EXIT_TOOL, MAP_TOOL, REDUCE_TOOL};
use std::collections::BTreeSet;

/// Everything loadable a plan step can call, resolvable without
/// connecting to anything. Built from config by the CLI runtime.
#[derive(Debug, Default, Clone)]
pub struct ToolCatalog {
    /// Full namespaced names of the enabled pack tools (`builtin__x`).
    pub builtin_tools: BTreeSet<String>,
    /// Full namespaced names of the loadable user tools (`user__x`).
    pub user_tools: BTreeSet<String>,
    /// Identifiers of loadable plan documents (callable as `plan__<id>`).
    pub plans: BTreeSet<String>,
    /// MCP server names configured under `[mcp.*]`.
    pub mcp_servers: BTreeSet<String>,
}

/// The outcome of resolving a plan's tool names against a catalog.
#[derive(Debug, Default)]
pub struct CatalogCheck {
    /// Names that cannot resolve here — running the plan would fail.
    pub errors: Vec<String>,
    /// Steps using an MCP server that is declared in `requires_servers`
    /// but not configured. The plan *file* is fine — the declaration is
    /// how portable plans name their dependencies — but in this
    /// environment the plan is hidden from the catalog and cannot run.
    pub notes: Vec<String>,
}

impl CatalogCheck {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Resolve one document's step tools (control bodies included) against
/// the catalog. `requires_servers` downgrades declared-but-unconfigured
/// MCP servers from errors to notes — see [`CatalogCheck::notes`].
pub fn resolve_plan_tools(doc: &PlanDoc, catalog: &ToolCatalog) -> CatalogCheck {
    let mut check = CatalogCheck::default();
    for (step_id, tool) in step_tools(&doc.steps) {
        check_tool(&step_id, &tool, &doc.requires_servers, catalog, &mut check);
    }
    check
}

/// [`resolve_plan_tools`], following `plan__*` references into the loaded
/// catalog documents (transitively, each once) so a composed plan fails
/// before its first step rather than at the sub-plan call. Sub-plan
/// problems are prefixed with the plan they belong to.
pub fn resolve_plan_tools_deep(
    doc: &PlanDoc,
    all_docs: &[PlanDoc],
    catalog: &ToolCatalog,
) -> CatalogCheck {
    let mut check = resolve_plan_tools(doc, catalog);
    let mut visited: BTreeSet<&str> = BTreeSet::from([doc.identifier.as_str()]);
    let mut queue: Vec<&str> = plan_refs(&doc.steps);
    while let Some(identifier) = queue.pop() {
        if !visited.insert(identifier) {
            continue;
        }
        // Unknown identifiers were already reported as errors above.
        let Some(sub) = all_docs.iter().find(|d| d.identifier == identifier) else {
            continue;
        };
        let sub_check = resolve_plan_tools(sub, catalog);
        let prefix = |p: String| format!("plan '{identifier}': {p}");
        check
            .errors
            .extend(sub_check.errors.into_iter().map(prefix));
        check.notes.extend(sub_check.notes.into_iter().map(prefix));
        queue.extend(plan_refs(&sub.steps));
    }
    check
}

/// Resolve bare steps (no document context) — the pre-execution guard in
/// [`super::Pipeline::run_explicit`]. No `requires_servers` leniency:
/// these steps are about to run, so every unconfigured server is an error.
pub fn resolve_step_tools(plan: &Plan, catalog: &ToolCatalog) -> Vec<String> {
    let mut check = CatalogCheck::default();
    for (step_id, tool) in step_tools(plan) {
        check_tool(&step_id, &tool, &[], catalog, &mut check);
    }
    check.errors
}

fn check_tool(
    step_id: &str,
    tool: &str,
    requires_servers: &[String],
    catalog: &ToolCatalog,
    check: &mut CatalogCheck,
) {
    match tool {
        EXIT_TOOL | DECIDE_TOOL | MAP_TOOL | REDUCE_TOOL | "plan_and_execute" => {}
        _ if super::plan::workbench_tool_problem(tool).is_some() => {
            // The static layer rejects these with the full explanation;
            // repeat it here so a catalog-only caller still fails.
            check.errors.push(format!(
                "step {step_id}: {}",
                super::plan::workbench_tool_problem(tool).unwrap()
            ));
        }
        _ if tool.starts_with("plan__") => {
            let identifier = &tool["plan__".len()..];
            if !catalog.plans.contains(identifier) {
                check.errors.push(format!(
                    "step {step_id}: unknown plan '{identifier}' ({tool}) — not \
                     in the plan catalog (missing, failed to load, or hidden by \
                     an unconfigured requires_servers entry)"
                ));
            }
        }
        _ if tool.starts_with(crate::user_tools::BUILTIN_TOOL_PREFIX) => {
            if !catalog.builtin_tools.contains(tool) {
                check.errors.push(format!(
                    "step {step_id}: unknown builtin tool '{tool}' — not \
                     provided by the enabled tool packs ([tools].packs)"
                ));
            }
        }
        _ if tool.starts_with(crate::user_tools::USER_TOOL_PREFIX) => {
            if !catalog.user_tools.contains(tool) {
                check.errors.push(format!(
                    "step {step_id}: unknown user tool '{tool}' — no such tool \
                     loads from [tools].paths"
                ));
            }
        }
        _ => {
            // MCP `server__tool`: verify the server; the tool itself is
            // only knowable by connecting. Bare non-control names are the
            // static layer's problem — skip them here.
            let Some((server, _)) = tool.split_once("__") else {
                return;
            };
            if catalog.mcp_servers.contains(server) {
                return;
            }
            let message = format!(
                "step {step_id}: tool '{tool}' needs MCP server '{server}', \
                 which is not configured under [mcp.{server}]"
            );
            if requires_servers.iter().any(|s| s == server) {
                check
                    .notes
                    .push(format!("{message} (declared in requires_servers)"));
            } else {
                check.errors.push(message);
            }
        }
    }
}

/// Every `(step id, tool name)` pair in a plan, control-step bodies
/// included. Bodies that fail to parse are skipped — the static layer
/// reports those.
fn step_tools(plan: &Plan) -> Vec<(String, String)> {
    let mut tools = Vec::new();
    for step in plan {
        tools.push((step.id.clone(), step.tool_name.clone()));
        let body_keys: &[&str] = match step.tool_name.as_str() {
            DECIDE_TOOL => &["then", "else"],
            MAP_TOOL | REDUCE_TOOL => &["do"],
            _ => &[],
        };
        for key in body_keys {
            let Some(raw) = step.input.get(*key) else {
                continue;
            };
            match parse_branch(key, raw) {
                Ok(Branch::Call(call)) => tools.push((step.id.clone(), call.tool_name)),
                Ok(Branch::Steps(steps)) => {
                    for body_step in steps {
                        tools.push((format!("{}/{}", step.id, body_step.id), body_step.tool_name));
                    }
                }
                Err(_) => {}
            }
        }
    }
    tools
}

/// The distinct `plan__*` identifiers a plan's steps (bodies included)
/// reference.
fn plan_refs(plan: &Plan) -> Vec<&str> {
    let mut refs = Vec::new();
    for step in plan {
        if let Some(identifier) = step.tool_name.strip_prefix("plan__") {
            refs.push(identifier);
        }
        let body_keys: &[&str] = match step.tool_name.as_str() {
            DECIDE_TOOL => &["then", "else"],
            MAP_TOOL | REDUCE_TOOL => &["do"],
            _ => &[],
        };
        for key in body_keys {
            let Some(raw) = step.input.get(*key) else {
                continue;
            };
            collect_plan_refs(raw, &mut refs);
        }
    }
    refs
}

fn collect_plan_refs<'a>(raw: &'a serde_json::Value, refs: &mut Vec<&'a str>) {
    let tool_of = |value: &'a serde_json::Value| {
        value
            .get("toolName")
            .or_else(|| value.get("tool_name"))
            .and_then(serde_json::Value::as_str)
    };
    match raw {
        serde_json::Value::Object(_) => {
            if let Some(identifier) = tool_of(raw).and_then(|t| t.strip_prefix("plan__")) {
                refs.push(identifier);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(identifier) = tool_of(item).and_then(|t| t.strip_prefix("plan__")) {
                    refs.push(identifier);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ToolCatalog {
        ToolCatalog {
            builtin_tools: BTreeSet::from(["builtin__infer".to_string()]),
            user_tools: BTreeSet::from(["user__git_log".to_string()]),
            plans: BTreeSet::from(["urgent_issues".to_string()]),
            mcp_servers: BTreeSet::from(["linear".to_string()]),
        }
    }

    fn doc(yaml: &str) -> PlanDoc {
        serde_yaml::from_str(yaml).unwrap()
    }

    const OK_DOC: &str = r#"
identifier: demo
name: Demo
description: demo
requires_servers: [linear]
steps:
  - id: E0
    tool_name: linear__list_issues
    input: {}
  - id: E1
    tool_name: builtin__infer
    input: { instruction: x, data: "{{E0}}" }
  - id: E2
    tool_name: user__git_log
    input: {}
  - id: E3
    tool_name: plan__urgent_issues
    input: {}
  - id: E4
    tool_name: exit
    input: { status: success, message: done }
"#;

    #[test]
    fn resolvable_names_in_every_namespace_pass() {
        let check = resolve_plan_tools(&doc(OK_DOC), &catalog());
        assert!(check.is_ok(), "{:?}", check.errors);
        assert!(check.notes.is_empty(), "{:?}", check.notes);
    }

    #[test]
    fn unknown_names_error_per_namespace() {
        let bad = OK_DOC
            .replace("builtin__infer", "builtin__nope")
            .replace("user__git_log", "user__nope")
            .replace("plan__urgent_issues", "plan__nope");
        let check = resolve_plan_tools(&doc(&bad), &catalog());
        assert_eq!(check.errors.len(), 3, "{:?}", check.errors);
        assert!(check.errors[0].contains("unknown builtin tool 'builtin__nope'"));
        assert!(check.errors[0].contains("[tools].packs"));
        assert!(check.errors[1].contains("unknown user tool 'user__nope'"));
        assert!(check.errors[2].contains("unknown plan 'nope'"));
    }

    #[test]
    fn unconfigured_undeclared_server_is_an_error() {
        let bad = OK_DOC.replace("linear__list_issues", "github__search_issues");
        let check = resolve_plan_tools(&doc(&bad), &catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(
            check.errors[0].contains("MCP server 'github'"),
            "{}",
            check.errors[0]
        );
        assert!(check.errors[0].contains("[mcp.github]"));
    }

    #[test]
    fn declared_but_unconfigured_server_is_a_note_not_an_error() {
        let declared = OK_DOC
            .replace("requires_servers: [linear]", "requires_servers: [github]")
            .replace("linear__list_issues", "github__search_issues");
        let check = resolve_plan_tools(&doc(&declared), &catalog());
        assert!(check.is_ok(), "{:?}", check.errors);
        assert_eq!(check.notes.len(), 1, "{:?}", check.notes);
        assert!(check.notes[0].contains("declared in requires_servers"));
    }

    #[test]
    fn workbench_tools_are_rejected() {
        let bad = OK_DOC.replace("linear__list_issues", "workbench__read_file");
        let check = resolve_plan_tools(&doc(&bad), &catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(
            check.errors[0].contains("workbench__"),
            "{:?}",
            check.errors
        );
        assert!(check.errors[0].contains("not available in the plan runtime"));
    }

    #[test]
    fn control_step_bodies_are_resolved_too() {
        let body_doc = doc(r#"
identifier: bodies
name: Bodies
description: control bodies
steps:
  - id: E0
    tool_name: linear__list_issues
    input: {}
  - id: E1
    tool_name: decide
    input:
      if: { value: "{{E0.count}}", op: gt, to: 0 }
      then: { toolName: ghost__triage, input: {} }
      else:
        - id: B0
          toolName: user__nope
          input: {}
  - id: E2
    tool_name: map
    input:
      over: "{{E0.issues}}"
      do: { toolName: builtin__nope, input: {} }
"#);
        let check = resolve_plan_tools(&body_doc, &catalog());
        assert_eq!(check.errors.len(), 3, "{:?}", check.errors);
        assert!(check.errors[0].contains("ghost"), "{}", check.errors[0]);
        assert!(check.errors[1].contains("E1/B0"), "{}", check.errors[1]);
        assert!(
            check.errors[2].contains("builtin__nope"),
            "{}",
            check.errors[2]
        );
    }

    #[test]
    fn deep_resolution_follows_plan_references() {
        let sub = doc(r#"
identifier: urgent_issues
name: Urgent
description: sub-plan with a defect
steps:
  - id: E0
    tool_name: ghost__list
    input: {}
"#);
        let check = resolve_plan_tools_deep(&doc(OK_DOC), &[sub], &catalog());
        assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
        assert!(
            check.errors[0].starts_with("plan 'urgent_issues':"),
            "{}",
            check.errors[0]
        );
        assert!(check.errors[0].contains("ghost"));
    }

    #[test]
    fn deep_resolution_survives_plan_cycles() {
        let a = doc(r#"
identifier: a
name: A
description: calls b
steps:
  - id: E0
    tool_name: plan__b
    input: {}
"#);
        let b = doc(r#"
identifier: b
name: B
description: calls a
steps:
  - id: E0
    tool_name: plan__a
    input: {}
"#);
        let mut catalog = catalog();
        catalog.plans.extend(["a".to_string(), "b".to_string()]);
        let check = resolve_plan_tools_deep(&a, &[a.clone(), b], &catalog);
        assert!(check.is_ok(), "{:?}", check.errors);
    }

    #[test]
    fn step_tool_resolution_has_no_requires_servers_leniency() {
        let declared = OK_DOC
            .replace("requires_servers: [linear]", "requires_servers: [github]")
            .replace("linear__list_issues", "github__search_issues");
        let problems = resolve_step_tools(&doc(&declared).steps, &catalog());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("MCP server 'github'"));
    }
}
