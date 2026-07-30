//! Characterization tests for the machine-facing CLI contract.
//!
//! These pin the behavior documented in `docs/reference/scripting-contract.mdx`
//! by running the real binary. They exist so that the planned refactor of
//! command output (every command returning a value instead of printing, in
//! service of an MCP server) can be shown to preserve behavior rather than
//! merely believed to. See `MCP-SERVER-ROADMAP.md`.
//!
//! Nothing here needs credentials or a network: fixture plans use the `data`
//! pack's pure `builtin__reshape` tool.

mod support;

use support::{Scratch, BROKEN_PLAN, ECHO_PLAN, GATE_PLAN};

// ---------------------------------------------------------------- streams --

#[test]
fn stdout_carries_only_the_deliverable() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    // A successful validate has no deliverable — the verdict is human-facing.
    scratch
        .graph(&["plan", "validate", "echo_ok"])
        .code_is(0)
        .stdout_empty()
        .stderr_contains("ok: 'echo_ok'");

    // A mutating command without --json likewise reports on stderr only.
    scratch
        .graph(&["plan", "new", "scratch_plan"])
        .code_is(0)
        .stdout_empty()
        .stderr_contains("scratch_plan.yaml");
}

#[test]
fn an_output_mode_plan_puts_bare_json_on_stdout() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    // Documented as the reason output-mode plans need no --json to be piped:
    // `graph plan run … | jq` must work directly.
    let run = scratch.graph(&["plan", "run", "echo_ok", r#"{"word":"hi"}"#]);
    run.code_is(0);
    assert_eq!(run.json(), serde_json::json!({"said": "hi"}));

    // Tool activity is progress, so it goes to stderr even though the same
    // command is writing a deliverable to stdout.
    run.stderr_contains("builtin__reshape");
}

#[test]
fn tool_progress_never_reaches_stdout_under_jsonl_events() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    let run = scratch.graph_env(
        &["plan", "run", "echo_ok", r#"{"word":"hi"}"#],
        &[("GRAPH_EVENTS", "jsonl")],
    );
    run.code_is(0);
    assert_eq!(run.json(), serde_json::json!({"said": "hi"}));

    // stderr switches from human progress to one JSON object per line.
    let events: Vec<serde_json::Value> = run
        .stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stderr line is not JSON ({e}): {line:?}"))
        })
        .collect();
    assert!(
        !events.is_empty(),
        "GRAPH_EVENTS=jsonl should emit an event feed"
    );
    assert!(
        events.iter().all(|event| event.get("event").is_some()),
        "every event carries an `event` discriminator: {events:?}"
    );
}

// ------------------------------------------------------------ exit codes ---

#[test]
fn a_missing_required_input_exits_three_and_prints_the_schema() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    // 3 is "needs input" — distinct from 1 so a caller can branch on it and
    // supply what's missing rather than treating the run as broken.
    scratch
        .graph(&["plan", "run", "echo_ok"])
        .code_is(3)
        .stdout_empty()
        .stderr_contains("needs inputs")
        // The schema goes to stderr precisely so the caller knows what to send.
        .stderr_contains("input schema:")
        .stderr_contains("word");
}

#[test]
fn a_fired_error_exit_gate_exits_four() {
    let scratch = Scratch::new();
    scratch.write_plan("gate", GATE_PLAN);

    // 4 means "the assertion in the plan is real, infrastructure is fine" —
    // the whole basis of the CI-checks cookbook. It must not collapse into 1.
    scratch
        .graph(&["plan", "run", "gate"])
        .code_is(4)
        .stdout_empty()
        .stderr_contains("no rows found");
}

#[test]
fn an_invalid_plan_exits_one_with_the_envelope_still_on_stdout() {
    let scratch = Scratch::new();
    scratch.write_plan("broken", BROKEN_PLAN);

    // "A non-zero code never truncates what was already written."
    let run = scratch.graph(&["plan", "validate", "broken", "--json"]);
    run.code_is(1);
    let envelope = run.json();
    assert_eq!(envelope["ok"], serde_json::json!(false));
    assert_eq!(
        envelope["problems"],
        serde_json::json!(["step E1 references E9, which is not `input` or an earlier step"])
    );
}

#[test]
fn an_unknown_tool_is_an_ordinary_error_not_an_envelope() {
    let scratch = Scratch::new();

    // A bad argument, not a domain rejection: message on stderr, exit 1, and
    // deliberately no envelope even though --json was passed.
    scratch
        .graph(&["tools", "show", "nope__missing", "--json"])
        .code_is(1)
        .stdout_empty()
        .stderr_contains("unknown tool");
}

// -------------------------------------------------------------- envelopes --

#[test]
fn plan_run_json_envelope_carries_the_documented_keys() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    let run = scratch.graph(&["plan", "run", "echo_ok", r#"{"word":"hi"}"#, "--json"]);
    run.code_is(0);
    let envelope = run.json();
    assert_eq!(envelope["plan"], serde_json::json!("echo_ok"));
    assert_eq!(envelope["output"], serde_json::json!({"said": "hi"}));
    assert_eq!(envelope["steps_executed"], serde_json::json!(1));
    // Every key is always present, so a caller can address it without probing.
    assert_eq!(envelope["answer"], serde_json::Value::Null);
    assert_eq!(envelope["exit"], serde_json::Value::Null);
}

#[test]
fn a_fired_gate_adds_an_exit_block_to_the_envelope() {
    let scratch = Scratch::new();
    scratch.write_plan("gate", GATE_PLAN);

    let run = scratch.graph(&["plan", "run", "gate", "--json"]);
    run.code_is(4);
    let envelope = run.json();
    let exit = &envelope["exit"];
    assert_eq!(exit["status"], serde_json::json!("error"));
    assert_eq!(exit["message"], serde_json::json!("no rows found"));
    assert_eq!(exit["step"], serde_json::json!("E2"));
    // A fired exit skips the remaining steps, so only E1 counted.
    assert_eq!(envelope["steps_executed"], serde_json::json!(1));

    // CHARACTERIZATION, NOT ENDORSEMENT: the contract documents `answer` as
    // the solver report ("null for output/silent plans") and this plan has no
    // solver, yet the gate message lands there. Pinned so the refactor can't
    // change it silently; see the drift list in MCP-SERVER-ROADMAP.md.
    assert_eq!(envelope["answer"], serde_json::json!("no rows found"));
}

#[test]
fn plan_validate_separates_fatal_problems_from_notes() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    let run = scratch.graph(&["plan", "validate", "echo_ok", "--json"]);
    run.code_is(0);
    let envelope = run.json();
    assert_eq!(envelope["ok"], serde_json::json!(true));
    assert_eq!(envelope["plan"], serde_json::json!("echo_ok"));
    assert_eq!(envelope["steps"], serde_json::json!(1));
    assert_eq!(envelope["problems"], serde_json::json!([]));
    assert_eq!(envelope["notes"], serde_json::json!([]));
}

#[test]
fn plan_list_reports_broken_and_valid_plans_separately() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);
    scratch.write_plan("broken", BROKEN_PLAN);

    let run = scratch.graph(&["plan", "list", "--json"]);
    run.code_is(0);
    let envelope = run.json();

    // A plan the catalog rejects is reported, not silently dropped...
    let plans = envelope["plans"].as_array().expect("plans array");
    assert_eq!(plans.len(), 1, "only the valid plan is in the catalog");
    assert_eq!(plans[0]["identifier"], serde_json::json!("echo_ok"));
    assert_eq!(plans[0]["steps"], serde_json::json!(1));

    // ...and a broken file never makes the whole listing fail.
    let skipped = envelope["skipped"].as_array().expect("skipped array");
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0]["path"]
        .as_str()
        .expect("skip path")
        .ends_with("broken.yaml"));
}

#[test]
fn the_tool_catalog_envelope_is_addressable_without_probing() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    let run = scratch.graph(&["tools", "list", "--json"]);
    run.code_is(0);
    let envelope = run.json();
    let tools = envelope["tools"].as_array().expect("tools array");
    assert_eq!(
        envelope["count"],
        serde_json::json!(tools.len()),
        "count must match the array it describes"
    );

    let reshape = tools
        .iter()
        .find(|tool| tool["name"] == serde_json::json!("builtin__reshape"))
        .expect("the data pack contributes builtin__reshape");
    assert_eq!(reshape["source"], serde_json::json!("builtin"));

    // A plan in the catalog is a callable tool — the fact the MCP server will
    // lean on to project plans as tools.
    assert!(
        tools
            .iter()
            .any(|tool| tool["name"] == serde_json::json!("plan__echo_ok")),
        "a catalog plan is exposed as plan__<identifier>: {tools:#?}"
    );
}

#[test]
fn tools_show_always_includes_every_schema_key() {
    let scratch = Scratch::new();

    let run = scratch.graph(&["tools", "show", "builtin__reshape", "--json"]);
    run.code_is(0);
    let envelope = run.json();
    assert_eq!(envelope["name"], serde_json::json!("builtin__reshape"));
    // "Every key is always present and null when absent, so a caller can
    // address .outputSchema without probing for it first."
    for key in ["inputSchema", "outputSchema", "outputExample", "readOnly"] {
        assert!(
            envelope.get(key).is_some(),
            "envelope must always carry {key}: {envelope:#}"
        );
    }
    assert!(envelope["inputSchema"]["properties"]["shape"].is_object());
}

// -------------------------------------------------------------- authoring --

#[test]
fn a_scaffolded_plan_is_ok_but_reports_what_remains_wrong() {
    let scratch = Scratch::new();

    let run = scratch.graph(&["plan", "new", "demo", "--json"]);
    run.code_is(0);
    let envelope = run.json();
    // The edit succeeded (ok) even though the plan is not yet runnable —
    // the two are deliberately different questions, which is what makes the
    // build-it-up-with-`step add` flow possible.
    assert_eq!(envelope["ok"], serde_json::json!(true));
    assert_eq!(envelope["identifier"], serde_json::json!("demo"));
    assert_eq!(
        envelope["problems"],
        serde_json::json!(["plan has no steps"])
    );
    assert!(scratch.exists(".graph/plans/demo.yaml"));
}

#[test]
fn a_rejected_edit_exits_one_with_the_problem_list_on_stdout() {
    let scratch = Scratch::new();
    scratch.graph(&["plan", "new", "demo", "--json"]).code_is(0);

    let run = scratch.graph(&[
        "plan",
        "step",
        "add",
        "demo",
        "E1",
        "builtin__reshape",
        r#"{"shape":{"a":"{{E7.x}}"}}"#,
        "--json",
    ]);
    run.code_is(1);
    let envelope = run.json();
    assert!(envelope["error"]
        .as_str()
        .expect("error message")
        .contains("edit rejected"));
    assert_eq!(
        envelope["problemsIntroduced"],
        serde_json::json!(["step E1 references E7, which is not `input` or an earlier step"])
    );

    // The draft is unchanged — a rejected edit must not half-apply.
    let yaml = scratch.read(".graph/plans/demo.yaml");
    assert!(!yaml.contains("E7"), "the file was modified: {yaml}");
}

#[test]
fn changing_the_identifier_writes_a_new_file_and_says_so() {
    let scratch = Scratch::new();
    scratch.graph(&["plan", "new", "demo", "--json"]).code_is(0);

    let run = scratch.graph(&["plan", "set", "demo", "identifier", "renamed", "--json"]);
    run.code_is(0);
    let envelope = run.json();
    assert_eq!(envelope["identifier"], serde_json::json!("renamed"));
    // `renamedFrom` is how a caller learns there is a leftover file to remove.
    assert!(envelope["renamedFrom"]
        .as_str()
        .expect("renamedFrom")
        .ends_with("demo.yaml"));
    assert!(scratch.exists(".graph/plans/renamed.yaml"));
    assert!(
        scratch.exists(".graph/plans/demo.yaml"),
        "the original is deliberately left in place"
    );
}

#[test]
fn a_step_add_envelope_reports_where_the_step_landed() {
    let scratch = Scratch::new();
    scratch.graph(&["plan", "new", "demo", "--json"]).code_is(0);

    let run = scratch.graph(&[
        "plan",
        "step",
        "add",
        "demo",
        "E1",
        "builtin__reshape",
        r#"{"shape":{"a":"1"}}"#,
        "--json",
    ]);
    run.code_is(0);
    let envelope = run.json();
    assert_eq!(envelope["ok"], serde_json::json!(true));
    assert_eq!(envelope["id"], serde_json::json!("E1"));
    assert_eq!(envelope["index"], serde_json::json!(0));
    assert_eq!(envelope["steps"], serde_json::json!(1));
}

// ------------------------------------------------------- CI annotations ----

#[test]
fn github_annotations_are_the_one_stdout_exception_and_only_on_failure() {
    let scratch = Scratch::new();
    scratch.write_plan("gate", GATE_PLAN);
    scratch.write_plan("echo_ok", ECHO_PLAN);

    // Failure: the annotation is a workflow command, and GitHub only parses
    // those from stdout — the sanctioned exception to stdout-is-deliverable.
    scratch
        .graph_env(&["plan", "run", "gate"], &[("GRAPH_EVENTS", "github")])
        .code_is(4)
        .stdout_contains("::error::no rows found");

    // Success: no annotation, stdout stays a clean deliverable.
    let ok = scratch.graph_env(
        &["plan", "run", "echo_ok", r#"{"word":"hi"}"#],
        &[("GRAPH_EVENTS", "github")],
    );
    ok.code_is(0);
    assert_eq!(ok.json(), serde_json::json!({"said": "hi"}));
}

#[test]
fn json_suppresses_github_annotations() {
    let scratch = Scratch::new();
    scratch.write_plan("gate", GATE_PLAN);

    // --json promises machine-parseable stdout, so it wins over the
    // annotation mode: the envelope must still parse.
    let run = scratch.graph_env(
        &["plan", "run", "gate", "--json"],
        &[("GRAPH_EVENTS", "github")],
    );
    run.code_is(4);
    assert!(
        !run.stdout.contains("::error::"),
        "an annotation would corrupt the envelope: {:?}",
        run.stdout
    );
    assert_eq!(run.json()["exit"]["status"], serde_json::json!("error"));
}
