# Tasks: decision models, sub-PR 1

Commands:
- Test: `mise run test`. Focused: `cargo test -p <crate> <filter>`.
- Lint: `mise run lint`.
- Build: `mise run build`.
- Review: `git add -N . && graph plan run graph_review_core --input base=origin/main`.

## Task 0: Dev branch and worktree

**Description:** Create `dev/decision-models` from `origin/main` in a worktree at `../graph-dm`. Copy in the gitignored `mise.local.toml`, which carries `TYPESAFE_API_KEY`. Commit the spec, plan, and this list at the branch root.

**Acceptance criteria:**
- [ ] The worktree exists on `dev/decision-models`, and `mise run build` passes there.
- [ ] `echo $TYPESAFE_API_KEY | wc -c` is non-zero under mise in the worktree.
- [ ] The spec, plan, and todo are committed (`ci:` type).

**Verification:** `git -C ../graph-dm status` is clean, and `mise run build` passes.
**Dependencies:** None.
**Files:** `SPEC-decision-models-1.md`, `decision-models-1-plan.md`, `decision-models-1-todo.md`.
**Scope:** XS.

## Task 1: Config v4

**Description:**
- Add `ProviderKind::Systemone`.
- Add optional `context_window: u32` on `ModelChoice` and `FallbackChoice`.
- Add `Role::Decider`, which never falls back to `default`.
- Set `CONFIG_FORMAT = 4` and add the no-op migration `systemone_provider_added`.
- Add the `tests/fixtures/v4/` golden set, including a `systemone.toml`.

**Acceptance criteria:**
- [ ] v1–v3 fixtures load identically, and the v4 fixtures load.
- [ ] `resolve("decider")` with no `[models.decider]` is `None`, not `default`.
- [ ] A config with `type = "systemone"` and `context_window` round-trips.

**Verification:** `cargo test -p graph-config`.
**Dependencies:** 0.
**Files:**
- `crates/graph-config/src/model.rs`
- `crates/graph-config/src/format.rs`
- `crates/graph-config/src/load.rs` (tests)
- `crates/graph-config/tests/fixtures/v4/*`

**Scope:** M.

## Task 2: `ModelCall` rename

**Description:** A pure refactor plus a `kind` field. Nothing produces `Decision` yet.
- `LlmCall` → `ModelCall { kind: ModelKind }`
- `LlmCallEvent` → `ModelCallEvent`
- `EventSink::llm_call` → `model_call`, on every sink including `TeeSink`
- The JSONL event becomes `model_call` with `kind`.
- OTLP `gen_ai.operation.name` comes from `kind`.

**Acceptance criteria:**
- [ ] No `llm_call` or `LlmCall` identifiers remain (`rg -n 'llm_call|LlmCall' crates` is empty).
- [ ] JSONL emits `{"event":"model_call","kind":"chat",…}`, and the existing telemetry tests pass with `operation=chat`.

**Verification:** `mise run test`, `mise run lint`.
**Dependencies:** 0.
**Files:**
- `graph-llm/src/metering.rs`
- `graph-core/src/{usage.rs,agent/events.rs}`
- `graph-cli/src/{telemetry.rs,output.rs,mcp_server/progress.rs,workbench/chat.rs}`

**Scope:** L. This is the accepted exception: an atomic, compiler-driven rename.

## Checkpoint A
- [ ] Test and lint are green.
- [ ] A `GRAPH_EVENTS=jsonl` run on an existing plan shows `model_call` events.

## Task 3: Wire types and the `systemone` client

**Description:**
- In `graph-llm/src/decision.rs`: `DecisionRequest`, `Question` (`Likelihood`/`Choice`/`Score`), `Answer`, and `DecisionResponse`.
- In `providers/systemone.rs`: `POST {base}/v1/systemone`, with optional Bearer auth and `with_retries`.
  - 401/422 are permanent errors; 429/529 are transient.
  - `likelihood` ↔ `noul` is translated only in this file.
- Stub-server tests.
- An `#[ignore]` live smoke test against `jev-latest`.

**Acceptance criteria:**
- [ ] The request body matches the documented shape, and `noul` appears only in `systemone.rs` (`rg -n noul crates` shows only that file).
- [ ] The `answers` parse works for likelihood and choice. The auth header is present and absent as configured. The 401/422/429/529 mapping is correct.
- [ ] The live smoke test passes: `cargo test -p graph-llm systemone_live -- --ignored`.

**Verification:** `cargo test -p graph-llm`, plus the ignored live test.
**Dependencies:** 0. It can run in parallel with Tasks 1–2.
**Files:** `graph-llm/src/{decision.rs,providers/systemone.rs,providers/mod.rs,lib.rs,error.rs}`.
**Scope:** M.

## Task 4: Router decision path

**Description:**
- `ModelRouter` gains a decision-provider map, built in `from_config`.
- Startup checks:
  - the chat, planner, solver, repair, and judge roles (and the `default` they fall back to) may not be `systemone`
  - `decider` must be `systemone`
  - fallback chains may not mix kinds
- `decide_named(name, req)` defaults to `decider`. A kind mismatch is an error.
- `FailoverDecider`.
- A pre-flight `context_window` check at bytes ÷ 4, with an error that names the role, the limit, and the estimate.
- A metered decider records `ModelCall { kind: Decision }` with decision request and response content.
- `with_providers` accepts mock decision providers.

**Acceptance criteria:**
- [ ] Each startup rejection has a test, and each message names the role.
- [ ] Failover goes from a failing hosted decider to a second decider. A fallback chain that mixes kinds is rejected at startup.
- [ ] An oversized state fails before any HTTP call, and the stub server sees no request.

**Verification:** `cargo test -p graph-llm`.
**Dependencies:** 1, 2, 3.
**Files:** `graph-llm/src/{roles.rs,failover.rs,metering.rs,error.rs,decision.rs}`.
**Scope:** M.

## Task 5: Decision span content

**Description:** OTLP generation spans for `kind: Decision` set `operation=decision`. They capture `{question, state, criteria}` as input and `{answers}` as output when `capture_content` is on, alongside cost, role, and site.

**Acceptance criteria:**
- [ ] A telemetry test asserts the span attributes for a decision call.
- [ ] Chat spans are unchanged.

**Verification:** `cargo test -p graph-cli --test telemetry`.
**Dependencies:** 4.
**Files:** `graph-cli/src/telemetry.rs`, `graph-cli/tests/telemetry.rs`.
**Scope:** S.

## Checkpoint B
- [ ] A live decision call through a scratch config (`[models.decider]` on TypeSafe) is metered in `--json` usage.
- [ ] Its span appears in Langfuse with `operation=decision` and the captured content.

## Task 6: `check_gate` and `DecideGate` on `exit`

**Description:**
- `evaluate_gate` → `check_gate`, over a gate enum: logic, `infer`, or `decide`.
- `DecideGate {question, state?, model?, criteria?, min_confidence?}`. `options` is rejected with "named cases arrive with `route`".
- The verdict is `probability ≥ min_confidence` (default 0.5).
- Runtime kind-mismatch errors in both directions.
- `CallSite` uses the real role instead of `"judge"`.
- The `exit` result is `{passed, verdict, probability}`, and a fired exit carries `probability` in its envelope.

**Acceptance criteria:**
- [ ] Mock-decider tests cover:
  - the `min_confidence` boundary (equal counts as yes)
  - `state` absent (sent as null) and present
  - both mismatch errors
  - the three gate keys being mutually exclusive
- [ ] Existing `infer` and `when` exit tests are unchanged and green.

**Verification:** `cargo test -p graph-core pipeline::`.
**Dependencies:** 4.
**Files:** `graph-core/src/pipeline/{condition.rs,exit.rs,mod.rs,body.rs}`.
**Scope:** M.

## Task 7: The gate on the `decide` step and on `filter`

**Description:**
- `DecideSpec` and `FilterSpec` accept `decide`.
- The `decide` step result is `{branch, verdict, probability, result}`.
- The `filter` result gains `probabilities`, in `over` order, under `concurrency`.

**Acceptance criteria:**
- [ ] With concurrency 4, `probabilities` is index-aligned with `over`.
- [ ] A failure drains in-flight verdicts, as today, and names the item index.
- [ ] Only the chosen branch renders, as today.

**Verification:** `cargo test -p graph-core pipeline::`.
**Dependencies:** 6.
**Files:** `graph-core/src/pipeline/{decision.rs,filter.rs}`.
**Scope:** S/M.

## Task 8: Plan format v2 and validation

**Description:**
- `PLAN_FORMAT = 2` with a no-op migration.
- Create the `graph-core` fixture harness: `tests/fixtures/v1/` holds frozen copies of representative plans (including ones using `exit`/`decide`/`filter` with `infer`), and `v2/` holds their migrated twins plus `decide`-gate plans. Add `tests/formats.rs`, mirroring graph-config's.
- `plan validate` reports a kind mismatch when `model:` is a literal.

**Acceptance criteria:**
- [ ] The golden pairs load identically, and a v3 plan is refused by name.
- [ ] `graph plan validate` on the repo's own `.graph/plans/*.yaml` is still ok.
- [ ] A literal `model: judge` under `decide:` is reported as a problem.

**Verification:** `cargo test -p graph-core --test formats`, and `mise run run -- plan validate <fixture>`.
**Dependencies:** 7.
**Files:**
- `graph-core/src/format.rs`
- `graph-core/tests/formats.rs`
- `graph-core/tests/fixtures/{v1,v2}/*`
- `graph-cli/src/commands/plan_cmd.rs` (or `authoring`)

**Scope:** M.

## Task 9: Planner surface

**Description:**
- The tool defs for `exit`, `decide`, and `filter` describe the `decide` gate.
- `control_step_rules.md` steers toward `decide` only when a decision role is configured, and otherwise keeps `infer`.
- Update the steering tests.

**Acceptance criteria:**
- [ ] The `prompts.rs` steering tests assert the new phrases.
- [ ] Prompt text and field names stay aligned.

**Verification:** `cargo test -p graph-core prompts`.
**Dependencies:** 7.
**Files:** `graph-core/src/pipeline/{exit.rs,decision.rs,filter.rs,prompts.rs}`, `graph-core/src/pipeline/prompts/control_step_rules.md`.
**Scope:** S/M.

## Checkpoint C
- [ ] `mise run test` and `mise run lint` are green.
- [ ] Every repo `.graph/plans/*` runs unchanged with the new binary. Spot-check `format_drift` on a recent PR.

## Task 10: Docs parity

**Description:** Update these pages:
- `models-and-providers.mdx`:
  - the `systemone` provider
  - a "Decision models" section
  - the `decider` role
  - fix the "exactly three places" claim
- `configuration.mdx`
- `exit-gates.mdx`, `branching.mdx`, `selection.mdx`, `plan-schema.mdx`: the `decide` gate and the result fields
- The cost table in `execution-model.mdx`

**Acceptance criteria:**
- [ ] Every behavior from Tasks 1–9 is documented, and no page mentions `noul`.
- [ ] The local `docs_drift` run on the branch diff passes.

**Verification:** `graph plan run docs_drift --input base=origin/main --input head=HEAD`.
**Dependencies:** 7 (8 for the plan-schema details).
**Files:** the `docs/` pages above. Docs only, so the 5-file guideline is relaxed.
**Scope:** M.

## Task 11: Experiment harness and smoke run

**Description:**
- `experiments/decision-models/.graph/config.toml`: v4, with:
  - the TypeSafe provider and `[models.decider]`
  - `[telemetry]` pointed at Langfuse
  - the github and llm packs
  - `[plans].paths` covering both the local plans and the repo's `.graph/plans`
- `plans/docs_drift_decide.yaml` and `plans/format_drift_decide.yaml`. Each swaps the `infer` exit for a `decide` exit, with the diff as `state`, and uses `:/`-prefixed pathspecs.
- `prs.txt`: the last 10 merged PRs that touched `crates/`, with base and head SHAs.
- `run.sh`: loops over `prs.txt` and runs all four plans.

**Acceptance criteria:**
- [ ] On one PR known to change `crates/`, both v1 and v2 `docs_drift` reach the gate. E0's diff is non-empty, so there is no false "not applicable".
- [ ] Langfuse shows both traces, with the decision span complete per Checkpoint B.

**Verification:** `./experiments/decision-models/run.sh --one`, then check Langfuse.
**Dependencies:** 8.
**Files:** `experiments/decision-models/{.graph/config.toml,.graph/plans/*.yaml,prs.txt,run.sh}`.
**Scope:** S.

## Task 12: Review and open the sub-PR

**Description:** Run lint, test, and `graph_review_core`, then fix any blocker or likely-bug findings. Ask before pushing. Then open the PR into `dev/decision-models`, not `main`.

**Acceptance criteria:**
- [ ] Review findings are addressed or explicitly declined with the user.
- [ ] The PR is open against the dev branch, with `graph-checks` green.

**Verification:** `mise run lint && mise run test`, the review output, and the PR checks.
**Dependencies:** 10, 11.
**Files:** none new.
**Scope:** XS.

## Task 13: Full drift experiment

**Description:** Run `run.sh` over all 10 PRs, then write `experiments/decision-models/REPORT.md`. Per gate and per PR, it records:
- the `infer` verdict, the `decide` verdict, and the probability
- agreement
- cost and latency for each

It ends with a one-line conclusion per gate.

**Acceptance criteria:**
- [ ] 10 PRs × 2 gates × 2 variants are all present in Langfuse.
- [ ] The report gives an agreement rate, cost ratio, and latency ratio per gate, and lists every disagreement with links.

**Verification:** The report numbers reconcile with the Langfuse totals.
**Dependencies:** 12, or 11 if the experiment runs before the merge.
**Files:** `experiments/decision-models/REPORT.md`.
**Scope:** S.

## Checkpoint D
- [ ] All five spec success criteria are met. Review the report with Tyler before sub-PR 2 (`route` rename and `cases`).
