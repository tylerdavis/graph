# graph as an MCP server — roadmap

Goal: expose graph's plan pipeline over MCP (stdio first) so that any agent can
author graph plans and call them as tools. The CLI commands added for the plan
workbench already cover most of the surface; this document tracks what stands
between those commands and a server.

Status legend: ☐ not started · ◐ in progress · ☑ done

---

## Phase 0 — Characterization tests (prerequisite)

The refactor in Phase 1 rewrites how every command emits its result. Today the
machine-facing contract in `docs/reference/scripting-contract.mdx` — which
stream carries what, which exit code means what, which keys are in each
envelope — is enforced by **nothing but code review**. Lock it down first, then
the refactor is provably behavior-preserving.

### Coverage as of `main` (0a5c478)

| Area | Tests | What is actually asserted |
|---|---|---|
| `commands/listing.rs` | 10 | `tool_as_json`, `render_tool_listing` (pure helpers) |
| `commands/plan_edit.rs` | 7 | `metadata_patch`, `list_as_json`, `doc_as_json` — **not** `set`/`unset`/`step`/`new_plan`/`draft` |
| `commands/input.rs` | 5 | inline / `@file` / `-` resolution |
| `output.rs` | 3 | GHA escaping, `SilentExit` downcast round-trip |
| `commands/config_cmd.rs` | 1 | — |
| `workbench/*` (9 files) | many | the surface Phase 1 does *not* touch |
| **`commands/plan_cmd.rs`** | **0** | owns exit codes 3 and 4 |
| `tools_cmd`, `threads_cmd`, `shapes_cmd`, `mcp_cmd`, `ask`, `chat_cmd`, `runtime`, `cli`, `main` | 0 | — |

No `crates/graph-cli/tests/` directory existed; no process-level test anywhere;
nothing asserted the streams contract. The only end-to-end coverage is
dogfooding in `.github/workflows/graph-checks.yaml` (two plans, needs
`ANTHROPIC_API_KEY`, smoke-tests success rather than asserting shape).

### Tasks

- ☑ Hermetic scratch harness (`tests/support/mod.rs`): temp dir as cwd **and**
  as `HOME`, so the `~/.config/graph` layer cannot leak the developer's real
  plans, MCP servers, or credentials into a test. `GRAPH_STORAGE=memory`.
- ☑ Zero-inference fixtures: plans built on the `data` pack's
  `builtin__reshape` run with no provider configured and no network.
- ☑ Streams contract: stdout carries only the deliverable; without `--json` a
  mutating command leaves stdout empty.
- ☑ Exit codes 0 / 1 / 3 / 4, each with the stdout payload still complete.
- ☑ `plan run` envelopes (solver-less output mode, exit-gate block).
- ☑ `plan validate` envelope: `problems` fatal, `notes` not.
- ☑ Authoring envelopes: `new`, `set`, `step add`, rejected edit, `renamedFrom`.
- ☑ `tools list/show --json`; unknown tool is an ordinary error, not an envelope.
- ☑ `GRAPH_EVENTS=github` — `::error::` on stdout only on failure, suppressed
  by `--json`.
- ☑ `GRAPH_EVENTS=jsonl` — stderr becomes one JSON object per line.
- ☑ `Cli::command().debug_assert()`.
- ☑ Wired into CI: `ci.yaml` already runs `cargo test --workspace`, and the
  suite needs no secrets and no network.

Landed as `crates/graph-cli/tests/{cli_contract.rs,support/mod.rs}` — 19 tests,
under a second, on the first `crates/graph-cli/tests/` directory in the repo.

The hermeticity guard is load-bearing and self-proving:
`plan_list_reports_broken_and_valid_plans_separately` asserts the catalog holds
exactly one plan. It only passes because `HOME` points at the temp dir; without
that, the developer's real `~/.config/graph/plans` (sprint_analysis,
project_status, urgent_issues) would leak in and the count would be wrong.

### Drift found while writing these

Recorded as characterizations of **current** behavior; decide separately
whether each is a bug or the doc is stale.

1. `plan run --json` on an error exit gate puts the gate message in `answer`,
   which `scripting-contract.mdx` documents as "solver report, null for
   output/silent plans". The plan here has no solver.
2. The `exit` envelope block carries an `output` key the contract doesn't list
   (documented as `{status, message, reason?, step}`).
3. `plan new` returns `ok: true` with a non-empty `problems` array (`plan has
   no steps`). Deliberate — "the edit applied" and "the plan is runnable" are
   different questions, and conflating them would break the
   `new` + `step add` flow — but it means `ok` alone is not a validity check,
   which the MCP tool descriptions will need to say out loud.

---

## Phase 0.5 — Retire revision-by-redraft ☑

Found while reviewing `plan new` vs `plan draft`: `merge_planner_output`'s
`Some(existing)` branch does `doc.steps = output.plan` — a wholesale
replacement wearing an edit's clothes. The command that read as "revise this
plan" was the only one that could silently destroy hand-tuned steps, and
redrafting to fix a plan never produced good results in practice.

The workbench system prompt already said "sequential edits are safer than a
wholesale re-draft" while the authoring skill routed structural problems
straight into `draft --from --feedback`. Two surfaces, opposite advice.

Removed the caller-supplied feedback slot everywhere:

- `Pipeline::draft_plan(query, existing)` — dropped `last_error`, and with it
  the drafting prompt's `Last Error` context variable. **Unrelated to the
  per-step retry**, which still injects validation problems inside the step
  loop and is what makes a returned draft statically valid.
- CLI `plan draft --feedback` gone; `--from` re-documented as "draft into this
  plan's identity", which is the good-identifier path the skill never taught.
- `workbench__draft_plan`'s `feedback` param gone; its description and both
  system prompts now say drafting always replaces every step.
- The replan path's own `last_error` (`PlannerPromptArgs`, fed from
  `state.last_error()`) is a different mechanism and is untouched.

Still open, deliberately deferred: whether `--from` and the workbench's
`fresh` flag survive the `new --goal` collapse below. They are the mechanism
that collapse would be built on, so they outlive the feedback flag.

## Phase 0.75 — Collapse `plan new` and `plan draft` ☐

One creation verb, drafting opt-in, identifier always explicit:

```bash
graph plan new <identifier> [--goal "<what it should do>"]
```

- no `--goal` → today's scaffold, zero inference, no credentials
- `--goal` → create and draft in one motion; retires `authoring::stated_name`,
  the ~40 lines that scrape `named X` out of goal prose to recover an argument
  the CLI could have taken as a flag
- mostly dissolves the `ok: true` + `problems: ["plan has no steps"]` oddity:
  the bare scaffold becomes the explicit hand-build path

Naming undecided (`new --goal` vs `create --goal` with `new` as alias). Blast
radius: `docs/plans/authoring.mdx`, `docs/reference/cli.mdx`,
`docs/reference/scripting-contract.mdx`, `skills/graph-plan-authoring/SKILL.md`.

## Phase 1 — Commands return values, not prints ☑

- ☑ Every command returns `commands::outcome::Outcome` (machine `body`,
  `rejected` for domain refusals, optional `raw` stdout deliverable).
  `outcome::report` is the only thing that writes a stream, and the only
  thing that reads `--json`.
- ☑ Filled the `--json` gaps this exposed: `tools test`, all of `threads`,
  `shapes show`, all of `mcp`, `config show`/`path`. Listings now agree on
  one shape — a named array plus a matching `count`.
- ☑ Progress sink — landed in Phase 4 as the MCP progress forwarder rather
  than a generic collector, which is the only consumer it turned out to need.
- ☑ Exit-code distinctions survive as structured content (see Phase 4).

Phase 0's suite passed unchanged across all of it, and caught one real
regression on the way: `raw` initially outranked `--json`, which would have
broken `plan list --json` and `plan show --json`.

Not converted, deliberately: `ask` and `chat_cmd` stream tokens to a
terminal and are not served over MCP.

## Phase 2 — The server ☑

- ☑ `graph mcp serve`, rmcp `server` + `transport-io`.
- ☑ **Not** a separate crate, contrary to the original plan. What is being
  served is `graph-cli`'s own command layer (`Runtime`, `commands::*`), none
  of which is extractable without turning the binary into a library first. A
  second crate would have meant a second implementation to keep in sync —
  the exact failure Phase 1 existed to remove. It lives in
  `graph-cli/src/mcp_server/`.
- ☑ Rooting via `--dir`. Load-bearing: MCP clients launch servers with an
  arbitrary cwd, so without it the catalog comes up empty.
- ☑ Concurrency: plan writes take a mutex, since MCP request handling is
  concurrent while read-modify-write authoring is not.
- ☑ Logging stays on stderr.
- ☐ `Runtime` is still built per call rather than once per session. Correct
  but wasteful — config and the plan catalog are re-read every time. Worth
  doing, but it must keep the shape cache read-fresh per planning attempt.
- ☐ Bridge to MCP `logging/setLevel`.

## Phase 3 — Tool surface ☑

- ☑ Authoring tools: `graph_plan_{list,show,validate,new,draft,set,unset}`,
  `graph_plan_step_{add,update,rename,rm}`, `graph_tools_{list,show,test}`.
  The `graph_` prefix keeps a plan named `tools_list` from colliding with
  the verb.
- ☑ Execution tools: each catalog plan as `plan_<identifier>`, carrying its
  own `input_schema`, with description + exemplars as the routing signal.
- ☑ `tools/list_changed` after every successful edit, and the list is read
  fresh per `tools/list` — so a plan authored in a session is callable in
  that session. Covered end to end by a test.
- ☑ Documented the recursion hazard (do not point `[mcp]` at `graph mcp
  serve`). Not enforced in code: the cycle detector works within a process.

## Phase 4 — Protocol semantics ☑

- ☑ Progress notifications, opt-in via `progressToken`, drained before the
  tool reply so notifications never land after the result they describe.
- ☑ Cancellation → `ExecutionGate::Abort`, stopping *between* tool calls
  rather than mid-side-effect.
- ☑ Exit codes as data: needs-input (CLI 3) returns the input schema so the
  agent can retry; a fired exit gate (CLI 4) returns the gate's message and
  step; a refused edit keeps its `problemsIntroduced` list. Argument errors
  stay JSON-RPC errors, because those are the caller's mistake.
- ☐ MCP elicitation for the needs-input case. The structured rejection
  already lets an agent retry, so this is an ergonomic upgrade, not a gap.

## Out of scope

The workbench TUI (`graph wb plan`) and the `chat` REPL. Interactive surfaces
do not project onto MCP.

## Still open

- **Phase 0.75** (below) — the `plan new` / `plan draft` collapse. Naming
  undecided; unblocked and cheap now that both return `Outcome`.
- `Runtime` built once per session instead of per call.
- Per-plan tools: whole catalog, or opt-in via a config allowlist? Serving
  everything is right for a small catalog and probably wrong for a large one.
- `ask`/`chat` over MCP: decided against. An agent calling graph does not
  need another agent, it needs the plans.
