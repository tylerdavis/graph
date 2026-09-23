mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;
use support::{Scratch, ECHO_PLAN};

struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn span_names(&self) -> Vec<String> {
        let body: Value = serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not OTLP JSON ({e}): {}", self.body));
        body["resourceSpans"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|r| r["scopeSpans"].as_array().into_iter().flatten())
            .flat_map(|s| s["spans"].as_array().into_iter().flatten())
            .filter_map(|span| span["name"].as_str().map(str::to_string))
            .collect()
    }

    fn resource_attribute(&self, key: &str) -> Option<String> {
        let body: Value = serde_json::from_str(&self.body).unwrap();
        body["resourceSpans"][0]["resource"]["attributes"]
            .as_array()?
            .iter()
            .find(|kv| kv["key"] == key)
            .and_then(|kv| kv["value"]["stringValue"].as_str())
            .map(str::to_string)
    }
}

fn otlp_receiver() -> (String, mpsc::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
                continue;
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.trim_end().split_once(':') {
                    let (k, v) = (k.trim().to_string(), v.trim().to_string());
                    if k.eq_ignore_ascii_case("content-length") {
                        content_length = v.parse().unwrap_or(0);
                    }
                    headers.push((k, v));
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let _ = reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = tx.send(Captured {
                path,
                headers,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
    });
    (base, rx)
}

fn expect_trace(rx: &mpsc::Receiver<Captured>) -> Captured {
    loop {
        let captured = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the exporter posts the trace before the process exits");
        if captured.path == "/v1/traces" {
            return captured;
        }
    }
}

#[test]
fn a_plan_run_exports_one_trace_with_the_configured_headers() {
    let (base, rx) = otlp_receiver();
    let scratch = Scratch::new();
    scratch.write_config(&format!(
        r#"
[tools]
packs = ["data"]

[telemetry]
endpoint = "{base}"
protocol = "http/json"
service_name = "graph-test"

[telemetry.headers]
Authorization = "Basic ${{GRAPH_TEST_OTLP_AUTH}}"
"x-langfuse-ingestion-version" = "4"

[telemetry.resource]
"deployment.environment" = "test"
"#
    ));
    scratch.write_plan("echo_ok", ECHO_PLAN);

    let run = scratch.graph_env(
        &["plan", "run", "echo_ok", r#"{"word":"hi"}"#],
        &[("GRAPH_TEST_OTLP_AUTH", "cGs6c2s=")],
    );
    run.code_is(0);
    assert_eq!(run.json(), json!({"said": "hi"}));

    let trace = expect_trace(&rx);
    assert_eq!(trace.header("authorization"), Some("Basic cGs6c2s="));
    assert_eq!(trace.header("x-langfuse-ingestion-version"), Some("4"));
    assert_eq!(
        trace
            .header("content-type")
            .map(|v| v.starts_with("application/json")),
        Some(true)
    );
    assert_eq!(
        trace.resource_attribute("service.name").as_deref(),
        Some("graph-test")
    );
    assert_eq!(
        trace
            .resource_attribute("deployment.environment")
            .as_deref(),
        Some("test")
    );
    let names = trace.span_names();
    assert!(
        !names.iter().any(|n| n == "builtin__reshape"),
        "a step's tool call is the step span, not a child: {names:?}"
    );
    for expected in ["echo_ok", "E1 builtin__reshape"] {
        assert!(
            names.iter().any(|n| n == expected),
            "no span named {expected:?} in {names:?}"
        );
    }
}

#[test]
fn the_standard_otel_variables_alone_turn_the_exporter_on() {
    let (base, rx) = otlp_receiver();
    let scratch = Scratch::new();
    scratch.write_plan("echo_ok", ECHO_PLAN);

    scratch
        .graph_env(
            &["plan", "run", "echo_ok", r#"{"word":"hi"}"#],
            &[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", base.as_str()),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
                ("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=abc, x-two=2"),
                ("OTEL_SERVICE_NAME", "graph-from-env"),
            ],
        )
        .code_is(0);

    let trace = expect_trace(&rx);
    assert_eq!(trace.header("x-api-key"), Some("abc"));
    assert_eq!(trace.header("x-two"), Some("2"));
    assert_eq!(
        trace.resource_attribute("service.name").as_deref(),
        Some("graph-from-env")
    );
    assert!(trace.span_names().iter().any(|n| n == "echo_ok"));
}

#[test]
fn an_unset_header_variable_disables_telemetry_and_says_so() {
    let (base, rx) = otlp_receiver();
    let scratch = Scratch::new();
    scratch.write_config(&format!(
        r#"
[tools]
packs = ["data"]

[telemetry]
endpoint = "{base}"
headers = {{ Authorization = "Basic ${{GRAPH_TEST_OTLP_AUTH_DOES_NOT_EXIST}}" }}
"#
    ));
    scratch.write_plan("echo_ok", ECHO_PLAN);

    let run = scratch.graph(&["plan", "run", "echo_ok", r#"{"word":"hi"}"#]);
    run.code_is(0)
        .stderr_contains("telemetry disabled")
        .stderr_contains("GRAPH_TEST_OTLP_AUTH_DOES_NOT_EXIST")
        .stderr_contains("telemetry.headers.Authorization");
    assert_eq!(run.json(), json!({"said": "hi"}));
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "nothing may be exported without the credential"
    );
}
