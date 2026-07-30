//! A hermetic scratch environment for driving the real `graph` binary.
//!
//! These are characterization tests: they pin the machine-facing contract in
//! `docs/reference/scripting-contract.mdx` — which stream carries what, which
//! exit code means what, which keys are in each envelope — so that refactors
//! of how commands emit their results are provably behavior-preserving.
//!
//! Hermeticity matters more than usual here. Config loads in two layers
//! (`~/.config/graph/config.toml`, then `./.graph/config.toml`), so a test
//! that only sets the working directory would still inherit the developer's
//! real plans, MCP servers, and API keys — and would pass or fail depending
//! on whose machine it ran on. [`Scratch`] therefore points `HOME` at the
//! temp dir too, which empties the global layer, and clears every `GRAPH_*`
//! variable that changes output.

#![allow(dead_code)] // Each test file uses a different subset of the helpers.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A temp directory that is simultaneously the working directory and `HOME`.
pub struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    /// A scratch with a config enabling the `data` pack — which supplies
    /// `builtin__reshape`, a pure tool needing no LLM, no network, and no
    /// credentials. Every fixture plan here is built on it so the suite runs
    /// in CI with no secrets.
    pub fn new() -> Self {
        let scratch = Self {
            dir: tempfile::tempdir().expect("create temp dir"),
        };
        std::fs::create_dir_all(scratch.path().join(".graph/plans")).expect("create .graph/plans");
        scratch.write_config("[tools]\npacks = [\"data\"]\n");
        scratch
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn write_config(&self, toml: &str) {
        std::fs::write(self.path().join(".graph/config.toml"), toml).expect("write config");
    }

    /// Write a plan into the scratch catalog. Returns its path.
    pub fn write_plan(&self, identifier: &str, yaml: &str) -> PathBuf {
        let path = self
            .path()
            .join(".graph/plans")
            .join(format!("{identifier}.yaml"));
        std::fs::write(&path, yaml).expect("write plan");
        path
    }

    pub fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path().join(relative)).expect("read scratch file")
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.path().join(relative).exists()
    }

    /// Run `graph <args>` inside the scratch.
    pub fn graph(&self, args: &[&str]) -> Run {
        self.graph_env(args, &[])
    }

    /// Run `graph <args>` with extra environment variables.
    pub fn graph_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graph"));
        command
            .args(args)
            .current_dir(self.path())
            .env("HOME", self.path())
            .env("GRAPH_STORAGE", "memory")
            // Anything inherited here would change what the assertions see.
            .env_remove("GRAPH_EVENTS")
            .env_remove("GRAPH_LOG")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY");
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().expect("run graph");
        Run {
            argv: args.iter().map(|a| a.to_string()).collect(),
            code: output
                .status
                .code()
                .expect("process returned a status code"),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// One completed `graph` invocation.
pub struct Run {
    pub argv: Vec<String>,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// Every assertion routes its failure message through here, so a broken
    /// expectation shows the command and both streams rather than
    /// `assertion failed: left == right`.
    fn context(&self) -> String {
        format!(
            "\n  command: graph {}\n  exit: {}\n  stdout: {:?}\n  stderr: {:?}",
            self.argv.join(" "),
            self.code,
            self.stdout,
            self.stderr
        )
    }

    #[track_caller]
    pub fn code_is(&self, expected: i32) -> &Self {
        assert_eq!(
            self.code,
            expected,
            "expected exit code {expected}{}",
            self.context()
        );
        self
    }

    /// The core of the streams contract: stdout carries the deliverable and
    /// nothing else, so a command with no deliverable must leave it empty.
    #[track_caller]
    pub fn stdout_empty(&self) -> &Self {
        assert!(
            self.stdout.is_empty(),
            "expected stdout to be empty — diagnostics belong on stderr{}",
            self.context()
        );
        self
    }

    #[track_caller]
    pub fn stderr_contains(&self, needle: &str) -> &Self {
        assert!(
            self.stderr.contains(needle),
            "expected stderr to contain {needle:?}{}",
            self.context()
        );
        self
    }

    #[track_caller]
    pub fn stdout_contains(&self, needle: &str) -> &Self {
        assert!(
            self.stdout.contains(needle),
            "expected stdout to contain {needle:?}{}",
            self.context()
        );
        self
    }

    /// Parse stdout as the JSON envelope the command promised.
    #[track_caller]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not valid JSON: {error}{}", self.context()))
    }
}

/// An output-mode plan: one pure `reshape` step, one required input. Runs
/// with zero inference, so it exercises `plan run` without credentials.
pub const ECHO_PLAN: &str = r#"
identifier: echo_ok
name: Echo OK
description: Reshape a plan input into an output document.
input_schema:
  type: object
  required: [word]
  properties:
    word: { type: string, description: The word to echo }
steps:
  - id: E1
    tool_name: builtin__reshape
    input:
      shape:
        said: "{{input.word}}"
output:
  said: "{{E1.said}}"
"#;

/// A plan whose error exit gate always fires — the exit-code-4 fixture.
pub const GATE_PLAN: &str = r#"
identifier: gate
name: Gate
description: Fires an error exit gate.
steps:
  - id: E1
    tool_name: builtin__reshape
    input:
      shape:
        count: 0
  - id: E2
    tool_name: exit
    input:
      when: { value: "{{E1.count}}", op: eq, to: 0 }
      status: error
      message: "no rows found"
output:
  n: "{{E1.count}}"
"#;

/// Statically invalid: E1 references a step that does not exist.
pub const BROKEN_PLAN: &str = r#"
identifier: broken
name: Broken
description: References a step that does not exist.
steps:
  - id: E1
    tool_name: builtin__reshape
    input:
      shape:
        said: "{{E9.nope}}"
output:
  said: "{{E1.said}}"
"#;
