//! End-to-end tests for `graph mcp serve`, spoken over real stdio JSON-RPC.
//!
//! These drive the shipped binary the way an MCP client does, because the
//! things most likely to break are exactly the things a unit test cannot see:
//! whether anything stray reaches stdout, whether a rejection survives as a
//! structured result, and whether a plan authored in a session becomes a
//! callable tool in that same session.

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use support::{Scratch, ECHO_PLAN};

/// A live MCP session against the binary.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// Server-initiated notifications seen while waiting for replies.
    notifications: Vec<String>,
}

impl Session {
    fn open(scratch: &Scratch) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_graph"))
            .args(["mcp", "serve", "--dir"])
            .arg(scratch.path())
            .current_dir(scratch.path())
            .env("HOME", scratch.path())
            .env("GRAPH_STORAGE", "memory")
            .env_remove("GRAPH_EVENTS")
            .env_remove("GRAPH_LOG")
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn graph mcp serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            notifications: Vec::new(),
        };
        session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "graph-tests", "version": "1"}
            }),
        );
        session.notify("notifications/initialized", json!({}));
        session
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("write to server");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Send a request and read until its reply, banking any notifications
    /// that arrive first.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read from server");
            assert!(read > 0, "server closed the connection awaiting {method}");
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                // stdout carries JSON-RPC and nothing else. Anything that
                // does not parse is a protocol-corrupting stray write, which
                // is the failure this whole refactor exists to prevent.
                Err(error) => panic!("non-JSON on stdout ({error}): {line:?}"),
            };
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                self.notifications.push(method.to_string());
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert!(
            reply.get("error").is_none(),
            "{tool} failed at the protocol level: {reply}"
        );
        reply["result"].clone()
    }

    /// The structured body of a tool result, plus whether it was flagged.
    fn call_parts(&mut self, tool: &str, arguments: Value) -> (Value, bool) {
        let result = self.call(tool, arguments);
        let is_error = result["isError"].as_bool().unwrap_or(false);
        (result["structuredContent"].clone(), is_error)
    }

    /// Call a tool asking for progress, and collect the notifications that
    /// arrive before the reply.
    fn call_with_progress(&mut self, tool: &str, arguments: Value) -> (Value, Vec<Value>) {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments,
                "_meta": {"progressToken": "tok-1"}
            }
        }));
        let mut progress = Vec::new();
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read");
            assert!(read > 0, "server closed awaiting {tool}");
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line:?}"));
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return (message["result"].clone(), progress);
            }
            if message.get("method") == Some(&json!("notifications/progress")) {
                progress.push(message["params"].clone());
            }
        }
    }

    fn tool_names(&mut self) -> Vec<String> {
        let reply = self.request("tools/list", json!({}));
        reply["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_server_advertises_tools_and_a_plan_becomes_one() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);
    let mut session = Session::open(&scratch);

    let names = session.tool_names();
    // The authoring half...
    for expected in [
        "graph_plan_list",
        "graph_plan_validate",
        "graph_plan_step_add",
        "graph_tools_list",
        "graph_tools_test",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected}: {names:?}"
        );
    }
    // ...and the execution half: this machine's plan, as a callable tool.
    assert!(
        names.contains(&"plan_echo_ok".to_string()),
        "a catalog plan must be exposed as a tool: {names:?}"
    );
}

#[test]
fn a_plan_tool_carries_the_plans_own_input_schema() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);
    let mut session = Session::open(&scratch);

    let reply = session.request("tools/list", json!({}));
    let tool = reply["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == json!("plan_echo_ok"))
        .expect("plan_echo_ok")
        .clone();
    // The calling agent gets the same argument contract graph enforces
    // internally — not a generic "run a plan" tool with a free-form blob.
    assert_eq!(tool["inputSchema"]["required"], json!(["word"]));
    assert_eq!(
        tool["inputSchema"]["properties"]["word"]["type"],
        json!("string")
    );
}

#[test]
fn running_a_plan_returns_its_output() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);
    let mut session = Session::open(&scratch);

    let (body, is_error) = session.call_parts("plan_echo_ok", json!({"word": "hi"}));
    assert!(!is_error, "{body}");
    assert_eq!(body["output"], json!({"said": "hi"}));
    assert_eq!(body["plan"], json!("echo_ok"));
    assert_eq!(body["steps_executed"], json!(1));
}

#[test]
fn a_missing_input_comes_back_as_a_result_carrying_the_schema() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);
    let mut session = Session::open(&scratch);

    // On the CLI this is exit code 3. Over MCP the code has nowhere to go,
    // so the distinction has to survive as data the agent can act on: it
    // should retry with the missing argument, not conclude the plan is broken.
    let (body, is_error) = session.call_parts("plan_echo_ok", json!({}));
    assert!(
        is_error,
        "a plan that cannot start is an error result: {body}"
    );
    assert_eq!(body["inputSchema"]["required"], json!(["word"]));
    assert!(
        body["problems"][0]
            .as_str()
            .unwrap_or_default()
            .contains("word"),
        "{body}"
    );
}

#[test]
fn a_fired_exit_gate_is_an_error_result_that_keeps_the_gate_message() {
    let scratch = Scratch::new();
    scratch.write_plan("gate", support::GATE_PLAN);
    let mut session = Session::open(&scratch);

    // CLI exit code 4. The plan worked; its assertion fired. That has to
    // read differently from "the plan is broken", and the message is the
    // whole point of writing the gate.
    let (body, is_error) = session.call_parts("plan_gate", json!({}));
    assert!(is_error, "{body}");
    assert_eq!(body["exit"]["status"], json!("error"));
    assert_eq!(body["exit"]["message"], json!("no rows found"));
    assert_eq!(body["exit"]["step"], json!("E2"));
}

#[test]
fn a_rejected_edit_keeps_its_problem_list() {
    let scratch = Scratch::new();
    let mut session = Session::open(&scratch);

    session.call(
        "graph_plan_new",
        json!({"identifier": "demo", "description": "d"}),
    );
    let (body, is_error) = session.call_parts(
        "graph_plan_step_add",
        json!({
            "target": "demo", "id": "E1", "tool": "builtin__reshape",
            "input": {"shape": {"a": "{{E9.x}}"}}
        }),
    );
    // The problem list is the useful part. Flattening it into a protocol
    // error would leave the model re-parsing prose to find out what to fix.
    assert!(is_error);
    assert_eq!(
        body["problemsIntroduced"],
        json!(["step E1 references E9, which is not `input` or an earlier step"])
    );
}

#[test]
fn an_agent_can_author_a_plan_and_then_call_it_in_the_same_session() {
    let scratch = Scratch::new();
    let mut session = Session::open(&scratch);

    // The whole thesis: build a capability, then use it, without a shell and
    // without restarting the server.
    session.call(
        "graph_plan_new",
        json!({"identifier": "made", "description": "Echo a word."}),
    );
    session.call(
        "graph_plan_step_add",
        json!({
            "target": "made", "id": "E1", "tool": "builtin__reshape",
            "input": {"shape": {"said": "{{input.word}}"}}
        }),
    );
    session.call(
        "graph_plan_set",
        json!({
            "target": "made", "attribute": "input_schema",
            "value": {"type": "object", "required": ["word"], "properties": {"word": {"type": "string"}}}
        }),
    );
    session.call(
        "graph_plan_set",
        json!({"target": "made", "attribute": "output", "value": {"said": "{{E1.said}}"}}),
    );

    let (verdict, _) = session.call_parts("graph_plan_validate", json!({"target": "made"}));
    assert_eq!(verdict["ok"], json!(true), "{verdict}");

    // It is now in the catalog, without a restart...
    assert!(session.tool_names().contains(&"plan_made".to_string()));
    // ...and the client was told, so it knows to re-read the list.
    assert!(
        session
            .notifications
            .iter()
            .any(|method| method == "notifications/tools/list_changed"),
        "authoring a plan must notify: {:?}",
        session.notifications
    );

    let (body, is_error) = session.call_parts("plan_made", json!({"word": "round-trip"}));
    assert!(!is_error, "{body}");
    assert_eq!(body["output"], json!({"said": "round-trip"}));
}

#[test]
fn json_arguments_reach_the_authoring_layer_unstringified() {
    let scratch = Scratch::new();
    let mut session = Session::open(&scratch);

    session.call(
        "graph_plan_new",
        json!({"identifier": "shapes", "description": "d"}),
    );
    // `exemplars` is a list on the CLI and must be a real array here; an
    // agent should never have to serialize JSON into a string to pass it.
    session.call(
        "graph_plan_set",
        json!({"target": "shapes", "attribute": "exemplars", "value": ["one", "two"]}),
    );
    let (body, _) = session.call_parts("graph_plan_show", json!({"target": "shapes"}));
    assert_eq!(body["exemplars"], json!(["one", "two"]), "{body}");
}

#[test]
fn tools_test_reports_a_tool_result_without_failing_the_call() {
    let scratch = Scratch::new();
    let mut session = Session::open(&scratch);

    let (body, is_error) = session.call_parts(
        "graph_tools_test",
        json!({"name": "builtin__reshape", "input": {"shape": {"a": "1"}}}),
    );
    assert!(!is_error);
    assert_eq!(body["result"], json!({"a": "1"}));
    assert_eq!(body["isError"], json!(false));
}

#[test]
fn a_running_plan_reports_progress_when_the_client_asks_for_it() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);
    let mut session = Session::open(&scratch);

    // Many clients time a tool call out after 60s of silence, and a plan can
    // run for minutes. Progress is what says it is still working.
    let (result, progress) = session.call_with_progress("plan_echo_ok", json!({"word": "hi"}));
    assert_eq!(result["structuredContent"]["output"], json!({"said": "hi"}));
    assert!(
        !progress.is_empty(),
        "a progressToken must produce notifications"
    );
    for note in &progress {
        assert_eq!(note["progressToken"], json!("tok-1"));
    }
    // The step being executed is named, so a human watching the client sees
    // the same thing `plan run` shows on a terminal.
    assert!(
        progress.iter().any(|note| note["message"]
            .as_str()
            .unwrap_or_default()
            .contains("builtin__reshape")),
        "{progress:?}"
    );
}

#[test]
fn a_plan_run_without_a_progress_token_stays_silent() {
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);
    let mut session = Session::open(&scratch);

    // MCP forbids notifying against a token the client never issued.
    session.call("plan_echo_ok", json!({"word": "hi"}));
    assert!(
        !session
            .notifications
            .iter()
            .any(|method| method == "notifications/progress"),
        "{:?}",
        session.notifications
    );
}

#[test]
fn the_server_advertises_the_capabilities_it_actually_implements() {
    let scratch = Scratch::new();
    let mut session = Session::open(&scratch);

    // `listChanged` is a promise: a client that trusts it will not re-read
    // the tool list unprompted, so advertising it without sending the
    // notification would strand every plan authored in-session.
    let reply = session.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "probe", "version": "1"}
        }),
    );
    assert_eq!(
        reply["result"]["capabilities"]["tools"]["listChanged"],
        json!(true),
        "{reply}"
    );
    assert_eq!(reply["result"]["serverInfo"]["name"], json!("graph"));
    // Instructions are the server's chance to teach the calling agent the
    // draft-once-then-edit loop before it guesses at it.
    let instructions = reply["result"]["instructions"]
        .as_str()
        .expect("instructions");
    assert!(
        instructions.contains("Never redraft to fix a plan"),
        "{instructions}"
    );
}

#[test]
fn an_unknown_tool_is_a_protocol_error_not_a_result() {
    let scratch = Scratch::new();
    let mut session = Session::open(&scratch);

    // A name that is not in the catalog is the caller's mistake, not a
    // domain outcome, so it belongs in the protocol's error channel.
    let reply = session.request(
        "tools/call",
        json!({"name": "graph_not_a_tool", "arguments": {}}),
    );
    assert!(reply.get("error").is_some(), "{reply}");
}
