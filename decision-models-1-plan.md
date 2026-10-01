# Implementation Plan: decision models, sub-PR 1 (drift experiment)

Spec: `SPEC-decision-models-1.md` (approved 2026-09-30). Tasks: `decision-models-1-todo.md`. Both files move to the dev-branch root in Task 0, and are deleted before the merge to `main`.

## Overview

This sub-PR adds a Jev-compatible decision provider, a `decider` role, Langfuse-complete decision spans, and a `decide` gate on `exit`, `filter`, and the binary `decide` step. It lands on `dev/decision-models`. It is done when a v2 clone of each drift plan runs side by side with the v1 original and can be compared in Langfuse.

## Architecture decisions

- **`DecisionProvider` is a sibling trait, not a `ChatProvider` mode.** The router holds two maps, and a kind mismatch is an error, never a fallback.
- **The rename lands as one atomic task (Task 2).** `LlmCall`, `LlmCallEvent`, and `llm_call` cross three crates, so splitting the rename would leave the tree uncompilable.
- **The wire client is proved before any plan-facing work (Task 3).** It includes a live, `#[ignore]`d smoke test against TypeSafe. It is the riskiest unknown, so it fails fast.
- **Sub-PR 1 establishes the plan-format fixture harness (Task 8).** `graph-core` has none yet, because this is the first plan bump. The v1 fixtures freeze today's files, and the v2 fixtures are their migrated twins.
- **The kind check is enforced at run time.** `plan validate` adds a static check only when `model:` is a literal.

## Task list

### Phase 0: Setup
- [x] Task 0: dev branch, worktree, spec and plan committed

### Phase 1: Foundation
- [x] Task 1: config v4 (`systemone` provider kind, `context_window`, `decider` role)
- [x] Task 2: `ModelCall` rename, `kind`, OTLP operation name, JSONL `model_call`

### Checkpoint A
- [ ] `mise run test` and `mise run lint` are green. Existing configs and plans behave identically.

### Phase 2: Decision provider
- [x] Task 3: wire types and the `systemone` client, with stub-server tests and a live smoke test
- [x] Task 4: router decision map, startup checks, `decide_named`, failover, `context_window`, metering
- [x] Task 5: decision span content in OTLP

### Checkpoint B
- [ ] A live `jev-latest` call through the router meters correctly, and its span carries `operation=decision` plus the captured input and output.

### Phase 3: The gate
- [x] Task 6: `check_gate` with `DecideGate`, on `exit`
- [x] Task 7: the gate on the `decide` step and on `filter` (`probabilities`)
- [x] Task 8: plan format v2, the fixture harness, and the `plan validate` kind check
- [ ] Task 9: planner surface (tool defs, `control_step_rules.md`, steering tests)

### Checkpoint C
- [ ] All three steps accept `decide`. v1 plans still load and run unchanged.

### Phase 4: Docs, experiment, review
- [ ] Task 10: docs parity
- [ ] Task 11: experiment harness, plus a one-PR smoke run to Langfuse
- [ ] Task 12: review and open the sub-PR into `dev/decision-models` (ask first)
- [ ] Task 13: the full drift experiment and its report

### Checkpoint D (done)
- [ ] All five spec success criteria are met. The report gives agreement, cost, and latency per gate.

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Jev misjudges the drift gates (hard, nuanced questions over long diffs) | High: it undermines the premise | It is surfaced, not hidden. Task 13 reports disagreement per PR. Tune `question`/`criteria`/`min_confidence` before concluding. |
| The live response shape differs from the docs | Med | The Task 3 live smoke test runs before anything depends on the types. |
| Atomic rename churn (7+ files) | Low (mechanical) | A dedicated task with no behavior change. Compiler-driven. |
| The first plan bump has no fixture harness | Med | Task 8 builds it, mirroring `graph-config/tests/formats.rs`. |
| `[plans].paths` or pathspecs resolve differently than assumed from a subdirectory | Med: a silent "not applicable" exit | The Task 11 smoke run asserts that E0 returns a non-empty diff for a PR known to touch `crates/`. |
| Sub-PRs trigger `graph-checks` (`docs_drift`, `format_drift`) on the dev branch | Low | Docs land in the same sub-PR, and the version bumps are present. |
| A diff exceeds the state budget | Low (40 KB ≈ 10k tokens; the budget is 32k) | `context_window` fails loudly. Never truncate silently. |

## Parallelization

- Task 3 is independent of Tasks 1–2, so it can run in parallel.
- Task 10 can start once Task 7 settles the result shapes.
- Everything else is sequential.

## Open questions

None blocking.
