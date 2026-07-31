//! What a command produces, separated from how it is presented.
//!
//! A command's job is to resolve arguments, do the work, and describe the
//! result as one JSON value. Turning that value into bytes on a stream — an
//! envelope on stdout under `--json`, a one-liner on stderr without it — is a
//! presentation concern, and it belongs to whoever is driving the command.
//!
//! Keeping the two apart is what lets a second driver exist. A stdio MCP
//! server cannot let commands print: stdout is the JSON-RPC channel, so a
//! stray `println!` corrupts the protocol mid-session. Commands that return
//! [`Outcome`] can be served over MCP by mapping the same value into
//! structured content, with no second implementation to keep in sync.
//!
//! See `MCP-SERVER-ROADMAP.md`.

use crate::output::SilentExit;
use anyhow::Result;
use serde_json::Value;

/// A finished command: the machine-readable body, and whether it represents
/// a domain rejection.
///
/// `rejected` is not "an error occurred" — a bad argument or unreadable file
/// is an ordinary `anyhow` error and never gets this far. It means the
/// command ran, understood the request, and refused it: an edit that would
/// introduce validation problems, a plan that does not validate. Those carry
/// a full explanation in `body`, which is exactly what a caller needs, so
/// they must not be flattened into an error string.
pub struct Outcome {
    pub body: Value,
    pub rejected: bool,
    /// Text that *is* the deliverable rather than a description of one —
    /// `plan show`'s YAML, `plan list`'s `identifier\tname` lines. Printed to
    /// stdout verbatim when `--json` is off.
    ///
    /// Commands with no deliverable (every edit command) leave this `None`
    /// and get the one-line stderr summary instead, which is what keeps
    /// stdout clean under the streams contract.
    pub raw: Option<String>,
}

impl Outcome {
    /// The command did what was asked.
    pub fn ok(body: Value) -> Self {
        Self {
            body,
            rejected: false,
            raw: None,
        }
    }

    /// The command understood the request and refused it. Exits 1, with the
    /// body still delivered.
    pub fn rejected(body: Value) -> Self {
        Self {
            body,
            rejected: true,
            raw: None,
        }
    }

    /// A document the caller asked for. `text` is the terminal rendering;
    /// `body` is the same thing as an envelope, for `--json` and for drivers
    /// (MCP) that have no stdout to write to.
    pub fn raw(text: String, body: Value) -> Self {
        Self {
            body,
            rejected: false,
            raw: Some(text),
        }
    }

    /// Attach a hint for the human — "no threads yet, run `graph ask`".
    ///
    /// It lives in `body` rather than being printed at the call site so that
    /// it reaches a machine caller as data, and so a `--json` run does not
    /// also spray advice at stderr.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.body["note"] = Value::String(note.into());
        self
    }
}

/// Render an [`Outcome`] on the terminal's terms and produce the exit code.
///
/// `--json` promises a machine-parseable envelope on stdout *including on
/// rejection*, so a caller always gets the structured problem list instead of
/// having to scrape stderr. Without it, stdout stays free for deliverables
/// (the streams contract) and the human gets one line on stderr.
pub fn report(outcome: Outcome, json: bool) -> Result<()> {
    let Outcome {
        body,
        rejected,
        raw,
    } = outcome;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else if let Some(text) = raw {
        print!("{text}");
        // An empty listing has nothing to put on stdout, so the hint that
        // explains why is the only thing the human gets.
        if let Some(note) = body.get("note").and_then(Value::as_str) {
            eprintln!("{note}");
        }
    } else if rejected {
        let headline = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the edit was rejected");
        eprintln!("✗ {headline}");
        for key in [
            "problemsIntroduced",
            "preExistingProblems",
            "problems",
            "availableSteps",
            "availablePlans",
        ] {
            if let Some(items) = body.get(key).and_then(Value::as_array) {
                eprintln!("  {key}:");
                for item in items {
                    eprintln!("    - {}", render_item(item));
                }
            }
        }
    } else {
        match body.get("savedTo").and_then(Value::as_str) {
            Some(path) => eprintln!("✓ {path}"),
            None => eprintln!("✓ ok"),
        }
        if let Some(note) = body.get("note").and_then(Value::as_str) {
            eprintln!("  note: {note}");
        }
        if let Some(problems) = body.get("preExistingProblems").and_then(Value::as_array) {
            eprintln!("  still invalid:");
            for problem in problems {
                eprintln!("    - {}", render_item(problem));
            }
        }
    }
    if rejected {
        // The body is already on stdout (--json) or stderr; the exit code is
        // the only thing left to carry, and it travels back to `main` so the
        // command can finish unwinding first.
        return Err(SilentExit::code(1));
    }
    Ok(())
}

fn render_item(item: &Value) -> String {
    match item.as_str() {
        Some(text) => text.to_string(),
        None => item.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_rejection_carries_exit_one_without_being_an_error() {
        // The distinction the type exists for: a refused edit is a complete,
        // well-described result that happens to exit non-zero — not an
        // anyhow error with the explanation lost to a string.
        let outcome = Outcome::rejected(json!({"error": "edit rejected"}));
        assert!(outcome.rejected);
        let error = report(outcome, true).expect_err("a rejection exits non-zero");
        let exit = error
            .downcast::<SilentExit>()
            .expect("rejections travel as SilentExit so teardown still runs");
        assert_eq!(exit.code, 1);
    }

    #[test]
    fn an_ok_outcome_exits_zero() {
        let outcome = Outcome::ok(json!({"savedTo": "/plans/demo.yaml"}));
        assert!(!outcome.rejected);
        report(outcome, true).expect("an ok outcome does not exit non-zero");
    }

    #[test]
    fn a_raw_outcome_still_carries_an_envelope_for_drivers_without_stdout() {
        // An MCP server has no stdout to print YAML to, so `raw` alone would
        // strand it. Both representations travel together.
        let outcome = Outcome::raw("identifier: demo\n".into(), json!({"identifier": "demo"}));
        assert_eq!(outcome.raw.as_deref(), Some("identifier: demo\n"));
        assert_eq!(outcome.body["identifier"], json!("demo"));
    }
}
