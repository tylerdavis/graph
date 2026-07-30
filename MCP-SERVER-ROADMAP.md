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

## Phase 1 — Commands return values, not prints

The blocker for a stdio server: stdout is the JSON-RPC channel, and today
~72 `println!`s live in `crates/graph-cli/src/commands/`.

- ☐ Split every command into a pure `fn … -> Result<Value>` core with the CLI
  layer doing the rendering. `plan_edit.rs` already has the shape
  (`report(body, json, is_error)`, `doc_as_json`, `list_as_json`,
  `listing::tool_as_json`) — make it the only path.
- ☐ Fill the `--json` gaps this exposes: `tools test`, `threads *`,
  `shapes show`, `mcp *`. Decide which are worth exposing over MCP at all
  (`config init`, `threads rm` probably not).
- ☐ Third `EventSink` variant: a channel/collector sink (model:
  `workbench/`'s channel-backed sink) instead of a terminal writer.
- ☐ `SilentExit` codes become part of the returned value, so 3 and 4 survive
  as structured content rather than a process exit status.

Phase 0's suite must pass unchanged across this work. That is the whole point.

## Phase 2 — The server

- ☐ New crate `graph-mcp-server` (keep the client/server split clean;
  `graph-mcp` stays the client). `rmcp` is already a dependency at 2.2 —
  enable the `server` + `transport-io` features.
- ☐ `graph mcp serve` subcommand.
- ☐ Rooting: MCP clients launch servers with an arbitrary cwd, so the
  project `./.graph/` layer resolves to nothing useful. Needs
  `--config`/`--plans-dir` and/or honoring MCP `roots`. Without it the server
  silently serves an empty plan catalog.
- ☐ Lifecycle: build `Runtime` once for the server's life — but keep the
  shape cache read-fresh per planning attempt (existing invariant).
  `McpManager::shutdown()` must run on MCP close/SIGTERM before the tokio
  runtime drops, or graph's own child servers orphan.
- ☐ Concurrency: rmcp serves requests concurrently. FileStore's flock/append
  design tolerates it; verify `plan run` re-entrancy.
- ☐ Logging stays on stderr (`init_tracing` already does); optionally bridge
  to MCP `logging/setLevel`.

## Phase 3 — Tool surface

**Authoring tools** (static) — the plan-authoring skill as an MCP server:

- ☐ `graph_plan_draft`, `graph_plan_validate`, `graph_plan_show`,
  `graph_plan_list`, `graph_plan_new`, `graph_plan_set`, `graph_plan_step_*`
- ☐ `graph_tools_list`, `graph_tools_show`, `graph_tools_test`

**Execution tools** (dynamic) — the higher-value half:

- ☐ Project each catalog plan as its own MCP tool: `input_schema` becomes the
  MCP input schema, `description`/`exemplars` the routing signal.
- ☐ `tools/list_changed` when plan files change (or accept staleness and
  document it).
- ☐ Guard against pointing graph's `[mcp]` config at graph's own server: the
  pipeline's depth-8 cycle detection does not cross a process boundary.

## Phase 4 — Protocol semantics

- ☐ Progress notifications from the collector sink. Not optional: `plan draft`
  is ~30s and `plan run` can be minutes, against a common 60s client timeout.
- ☐ Cancellation → `ExecutionGate::Abort`, surfacing as
  `PipelineError::Aborted{state}` (the hook already exists).
- ☐ Exit codes → results: 3 (needs input) and 4 (gate assertion) must stay
  distinct as `isError` + structured content, not collapse to an error string.
- ☐ Consider MCP elicitation for the needs-input case.

## Out of scope

The workbench TUI (`graph wb plan`) and the `chat` REPL. Interactive surfaces
do not project onto MCP.

## Open questions

- Does the server expose `ask`/`chat` at all? It would make graph an agent
  inside an agent — probably not for v1.
- Per-plan tools: whole catalog, or opt-in via a config allowlist?
- Does `plan run` over MCP write to the real thread store, or always ephemeral?
