# Spec: decision models, sub-PR 1 (drift experiment)

Intent: `decision-models-intent.md`. Design: `please-research-jev-and-memoized-oasis.md`. Target branch: `dev/decision-models`, cut from `origin/main`.

## Assumptions (correct any of these before approval)

1. **One spec, no capability map.** The four pieces form a single dependency chain: provider → router → spans → gate. None of them ships usefully alone.
2. **Version bumps happen here, on the dev branch, and later sub-PRs continue them.** Config 3→4 adds `systemone` and `context_window`. Plan 1→2 adds the `decide` gate key. Sub-PR 2's rename lands inside plan v2, with no further bump.
3. **Token estimate is bytes ÷ 4.** `context_window` is checked against that estimate. The repo has no tokenizer, and the check exists to fail fast with a clear message, not to be exact.
4. **The gate kind check has two layers.**
   - `graph plan validate` checks a literal `model:` against the loaded config.
   - The runtime check at the step is authoritative, and it also covers templated `model:` values.
5. **Wire types are hand-written serde structs.** There is no TypeSafe SDK in Rust, and no new crates are needed: `reqwest`, `serde`, and `async-trait` are already in `graph-llm`.
6. **The drift gates run on PRs into the dev branch.** `graph-checks.yaml` has no `branches:` filter, so the docs-parity rule applies to every sub-PR.
7. **This spec lives outside the repo until the dev branch exists.** It sits in `~/.claude/plans/` for now. Once the branch exists, it is committed at the branch root and deleted before the merge to `main`.

## Objective

Let a CI plan swap an `infer` gate for a `decide` gate that calls a Jev-compatible decision model. Then compare the two in Langfuse.

**User story:** as the maintainer, I clone `docs_drift` as `docs_drift_decide`, replace the E5 `infer` exit with a `decide` exit, run both with a dev-branch binary on the same PRs, and see in Langfuse, per PR, both verdicts, the decision model's probability, cost, and latency.

## Scope

### Config (v4)

- `ProviderKind::Systemone`. `base_url` defaults to `https://api.typesafe.ai`. `api_key` is optional.
- An optional `context_window: u32` on `ModelChoice` and `FallbackChoice`.
- A new standard role, `decider`. `Role::Decider` never falls back to `default`.
- Startup checks in `ModelRouter::from_config`:
  - `chat`, `planner`, `solver`, `repair`, and `judge`, plus the `default` they fall back to, must not resolve to a `systemone` provider.
  - `decider` must resolve to `systemone`.
  - A fallback chain must not mix kinds.
- A no-op migration `systemone_provider_added`, and fixtures in `tests/fixtures/v4/`.

### graph-llm

- `providers/systemone.rs` implements `POST {base}/v1/systemone`:
  - optional Bearer auth
  - `with_retries`
  - 401/422 map to permanent errors; 429/529 are transient
- `DecisionProvider` trait with `DecisionRequest {model, state: Value, questions: BTreeMap<String, Question>}`.
  - `Question` covers `Likelihood {instructions, criteria}` and `Choice {instructions, criteria}`. `score` is parsed but not used yet.
  - graph's `Likelihood` maps to Jev's `noul` only in this file.
- `ModelRouter`:
  - a decision-provider map
  - `decide_named(name: Option<&str>, req)`, which defaults to `decider`
  - a kind-mismatch error naming the role and its kind
  - `FailoverDecider` for failover
  - a pre-flight `context_window` check
- `LlmCall` → `ModelCall`, gaining `kind: ModelKind {Chat, Decision}`. A metered decision wrapper records it.
  - `request_content` and `response_content` get decision variants: `{question(s), state, criteria}` and `{answers}`.

### graph-core

- `LlmCallEvent` → `ModelCallEvent`, gaining `kind`. `EventSink::llm_call` → `model_call`, on `TeeSink` too.
- Gate call sites pass the real role instead of the hard-coded `"judge"`: `decision.rs`, `filter.rs`, `mod.rs`, `body.rs`.
- `pipeline/condition.rs`:
  - `evaluate_gate` → `check_gate`.
  - A new `DecideGate {question, state?, model?, criteria?, min_confidence?}`.
  - `options` is rejected in this sub-PR with "named cases arrive with `route`".
- `ExitSpec`, `FilterSpec`, and `DecideSpec` gain `decide: Option<DecideGate>`. Exactly one gate key is allowed per step: `when`/`where`/`if`, `infer`, or `decide`.
- Verdict: `probability ≥ min_confidence`, where `min_confidence` defaults to 0.5. Step results:
  - exit: `{passed, verdict, probability}`
  - decide: `{branch, verdict, probability, result}`
  - filter: gains `probabilities`, in `over` order
- `infer` on a decision role, or `decide` on a chat role, is an error at run time, and at `plan validate` when `model:` is literal.
- Plan format 1→2 with a no-op migration, fixtures in `tests/fixtures/v2/`, and `check_step_keys` updated.
- Tool defs and `control_step_rules.md` document the `decide` gate. Planner steering tests are updated.

### graph-cli

- OTLP `model_call` sets `gen_ai.operation.name` from `kind`: `chat` or `decision`. Captured input and output use the decision shapes.
- The JSONL event becomes `"event": "model_call"` with `kind`.

### Docs, in this sub-PR

- `models-and-providers.mdx`:
  - the `systemone` provider
  - the "Decision models" section
  - the `decider` role
  - fix the "exactly three places" claim
- `configuration.mdx`: `type = "systemone"`, `context_window`, `[models.decider]`.
- `exit-gates.mdx`, `branching.mdx`, `selection.mdx`, `plan-schema.mdx`: the `decide` gate and the result fields.
- `file-versions.mdx`, only if it names behavior, which it shouldn't; it states the contract.
- The execution-model cost table.

### Out of scope

- The `route` rename
- `options` / `cases`
- `kind: decision` tools and `builtin__decide`
- `score` in any gate
- Planner acceleration
- Shadow mode
- Cloudflare
- Workbench N-way rendering; the binary junction is unchanged

## Commands

```bash
git fetch origin && git worktree add ../graph-dm -b dev/decision-models origin/main
cp mise.local.toml ../graph-dm/
(cd ../graph-dm/experiments/decision-models && ../../target/debug/graph plan run docs_drift_decide --input base=<sha> --input head=<sha>)
mise run build
mise run test
mise run lint
cargo test -p graph-config --test formats
cargo test -p graph-llm
cargo test -p graph-core pipeline::
git add -N . && graph plan run graph_review_core --input base=origin/main
```

## Project structure (files touched)

```
crates/graph-config/src/{model.rs,format.rs}      ProviderKind, context_window, Role::Decider, v4 migration
crates/graph-config/tests/fixtures/v4/            golden configs
crates/graph-llm/src/providers/systemone.rs       new: wire client + noul↔likelihood
crates/graph-llm/src/{decision.rs,roles.rs,failover.rs,metering.rs,error.rs,lib.rs}
crates/graph-core/src/pipeline/{condition.rs,exit.rs,filter.rs,decision.rs,body.rs,mod.rs}
crates/graph-core/src/{usage.rs,agent/events.rs,format.rs,doc.rs}
crates/graph-core/src/pipeline/prompts/control_step_rules.md
crates/graph-core/tests/fixtures/v2/              golden plans
crates/graph-cli/src/{telemetry.rs,output.rs,mcp_server/progress.rs}
docs/...                                          as listed above
```

## Code style

Match the surrounding crates, and write no comments in new code (user rule). Shape example:

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideGate {
    pub question: String,
    #[serde(default)]
    pub state: Option<Value>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub criteria: Option<LikelihoodCriteria>,
    #[serde(default)]
    pub min_confidence: Option<f64>,
}
```

- Conventional commits inside the dev branch.
- The final merge to `main` is typed per RELEASING.md: `feat(config)!` and `feat(plan)!`, split if the linter requires it.

## Testing strategy

- **Wire:** a stub `TcpListener` server, following the pattern in `crates/graph-cli/tests/telemetry.rs`. Cover:
  - a request body round-trip, including the `likelihood`→`noul` translation
  - the `answers` parse
  - the auth header present and absent
  - 401/422 permanent, 429/529 retried
- **Router:**
  - each startup-check rejection
  - fallback kind mixing
  - `decider` with no fallback to `default`
  - `context_window` over the limit, with an error that names the role, the limit, and the estimate
- **Gate:** mock decision providers via `with_providers`. Cover:
  - `decide` on exit, filter, and decide
  - the `min_confidence` boundary (equal counts as yes)
  - filter `probabilities` order under `concurrency`
  - both kind-mismatch errors
  - mutually exclusive keys
  - `options` rejected
- **Formats:** config v4 and plan v2 fixtures load, golden pairs are identical, and a newer file is refused.
- **Telemetry:** the span for a decision call has `gen_ai.operation.name=decision`, role, site, cost, and captured input/output.
- **Live:**
  - TypeSafe hosted first (`jev-latest`)
  - Ollaya on `localhost:11435`, to confirm the `answers` shape against a local server (after the hosted run; not blocking)
  - then the drift experiment itself: `docs_drift` against `docs_drift_decide`, and the same for format, over 10 or more past PRs, to Langfuse

## Boundaries

- **Always:**
  - work in the dev-branch worktree
  - keep `noul` out of every surface except `systemone.rs`
  - update docs in the same sub-PR
  - run `mise run lint` and `graph_review_core` before opening the sub-PR
- **Ask first:**
  - adding any crate dependency
  - touching `.github/` or the root `.graph/` on the dev branch (`experiments/decision-models/` is fine)
  - any change to `infer`'s existing behavior
  - pushing, or opening a PR
- **Never:**
  - pin the dev branch into CI
  - bump a file version a second time on the dev branch
  - silently truncate `state`
  - let `infer` or `decide` fall back across kinds
  - commit API keys; `${TYPESAFE_API_KEY}` only

## Success criteria

1. `mise run test` and `mise run lint` are green, and `graph_review_core` shows no blocker or likely-bug findings.
2. A v3 config with no `systemone` provider loads unchanged. Every existing v1 plan runs unchanged under the new binary.
3. With the `decider` role on a `systemone` provider, `docs_drift_decide` runs end to end, and its E5 span shows in Langfuse with:
   - `operation=decision`
   - `role=decider`
   - the step path
   - the question, the state, and the probability
   - input tokens, cost, and latency
4. A misconfigured role, whether `judge` on `systemone` or `decider` on `anthropic`, fails at startup with a message that names the role.
5. The drift experiment runs on a fixed PR set, and the report gives verdict agreement, cost, and latency per gate.

## Decisions (resolved 2026-09-30)

1. **Drift clones live in the repo, outside the CI-validated `.graph/`.**
   - Location: `experiments/decision-models/.graph/`. It holds:
     - `config.toml`: v4, with `[providers.typesafe] type = "systemone"`, `api_key = "${TYPESAFE_API_KEY}"`, `[models.decider]`, `[telemetry]` → Langfuse, packs `github` + `llm`, and `[plans].paths = [".graph/plans", "../../.graph/plans"]` (paths resolve from the working directory; confirm during setup)
     - `plans/docs_drift_decide.yaml` and `plans/format_drift_decide.yaml`
   - Runs use that directory as the working directory, so the v1 originals and the v2 clones share one binary and one config.
   - The clones change two things only:
     - the `infer` exit becomes a `decide` exit, with the diff as `state`
     - `builtin__git_diff` pathspecs use git's repo-root form (`:/crates/`), because pathspecs resolve relative to the working directory. `git_changed_files` already reports repo-root paths.
   - The directory is deleted before the merge to `main`.
2. **PR set:** the last 10 merged PRs that touched `crates/`, fixed once and recorded in `experiments/decision-models/prs.txt`.
3. **Endpoint:** TypeSafe hosted (`jev-latest`). `TYPESAFE_API_KEY` is already in the main checkout's gitignored `mise.local.toml`. Copy that file into the worktree, since gitignored files don't follow.
