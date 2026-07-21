# graph-cli

A Rust CLI agent with a plan-based execution engine. Cargo workspace, six crates. GitHub: `tylerdavis/graph`.

**Positioning: plans first.** The plan pipeline is the core product. "The workbench" means the plan workbench TUI (`graph wb plan`) — the review/debug surface for plans; the agent loop (`ask`/`chat`) is the conversational layer for probing tools, prototyping, and exercising plans. Frame features and docs accordingly, and keep that vocabulary: never call `ask`/`chat` "the workbench".

**The documentation under `docs/` is the canonical reference** for behavior, file formats, and CLI surface — read it before changing the corresponding code, and update it in the same PR as any behavior change. Start with `docs/getting-started/concepts.mdx` and `docs/architecture/execution-model.mdx`. Published via Mintlify from this repo (`docs.json` is the nav manifest; monorepo contentDirectory `docs/`; builds only trigger when `docs/` changes; the site is private).

## Build & test

Tooling is managed by mise; plain `cargo` works, including rust-analyzer.

```bash
mise run build          # cargo build
mise run test           # all workspace tests
mise run lint           # fmt --check + clippy -D warnings (run before committing)
mise run run -- <args>  # run the CLI
mise run install        # release build onto PATH
mise run release:patch  # cut a release (also :minor / :major) — see RELEASING.md
```

CI (`.github/workflows/ci.yaml`) runs lint + tests on ubuntu-24.04 and macos-15 for every push/PR touching non-docs paths; sccache keeps cache-key rotations from recompiling the world. Tags `v*` trigger `release.yaml`: binaries for macOS arm64 + Linux x86_64 with checksums, plus the `ghcr.io/tylerdavis/graph` container image (graph + git/jq/gh; `workflow_dispatch` with a `tag` input re-mints an image for an existing release).

**graph dogfoods itself on every PR** (`.github/workflows/graph-checks.yaml`): the `docs_drift` plan fails the check (exit 4) on undocumented crate behavior changes, and `pr_review` posts rubric'd findings as a PR comment — both run from the repo-carried `./.graph/` inside the release image (needs the `ANTHROPIC_API_KEY` repo secret). `.graph/` is partially tracked: config, plans, and tools are committed; everything else under it stays ignored. Expect the drift gate to hold you to the docs-parity invariant below.

## Crate map

| Crate | Contents |
|---|---|
| `graph-cli` | binary: clap tree, REPL, sinks (`TtySink`, `JsonlSink`), input resolution, runtime wiring, plan workbench TUI (`src/workbench/`: ratatui dual-pane, pure `update()` reducer, channel-backed sink, `UiGate`) |
| `graph-core` | agent loop, plan pipeline (incl. `exit` gates, plan composition), template engine, toolbox, user tools + bundled packs (`src/packs/`), Store/ToolRegistry traits, prompts, shape inference |
| `graph-llm` | ChatProvider trait + anthropic/openai_compat providers, retries with backoff, cross-provider failover (`fallbacks` on model entries), structured output + repair, ModelRouter (role → model) |
| `graph-mcp` | rmcp client manager (stdio + streamable HTTP), `server__tool` namespacing, graceful shutdown |
| `graph-store` | FileStore (plain JSON/JSONL files, default), MemoryStore (ephemeral/CI), RecordingRegistry (shape cache) |
| `graph-config` | layered TOML config + serde models (providers, model roles incl. `judge`, storage backends, plan/tool paths, tool packs) |

## Architecture facts that bite

- **Tool catalog namespacing**: `<server>__` (MCP), `user__` (user tools), `builtin__` (bundled packs), `plan__` (plan docs), plus reserved bare names `plan_and_execute`, `exit`, `decide`, `map`, and `reduce`. Everything shares one `ToolRegistry` catalog (`CompositeRegistry` merges sources); shape recording wraps the base. Packs (`[tools].packs`, YAML embedded in `graph-core/src/packs/`) reuse the user-tool format and registry with a different prefix; customizing one means copying it into a tools dir as a `user__` tool.
- **Plan invocation lives in the pipeline**, not the toolbox: `Pipeline::call_plan`/`call_planner` (boxed recursive futures). The toolbox is a thin adapter for the agent loop. **Plans compose**: `plan__*` and `plan_and_execute` are valid step tools; the pipeline call stack detects cycles and caps depth at 8.
- **Control steps** (`exit` gates, `decide` forks, `map`/`reduce` iteration) are executor-intercepted, never dispatched to a registry. `exit`/`decide` share one gate grammar (`pipeline/condition.rs`): a logical value/op/to condition (spelled `when` on exit, `if` on decide) or `infer` (yes/no verdict via the `judge` role). Fired exits skip remaining steps *and the solver*; error exits → process exit code 4, `is_error` plan-tool results. `decide`'s `then`/`else` and `map`/`reduce`'s `do` share one body grammar (`pipeline/body.rs`): a single call or inline step list, rendered lazily (only the gate / `over` renders up front; only the chosen branch renders at all; map/reduce bodies render per item with `item`/`index` pseudo-roots, plus `accumulator` on reduce). Body-step results stay scoped (never in `state.results` — the step cursor and replan merge key on it) but count in `steps_executed`. No control-step nesting inside bodies — bodies call `plan__*` for that. `map`'s `concurrency` is the pipeline's only parallelism: items run via ordered `buffered(n)` and failures drain in-flight items rather than cancel; `reduce` is sequential by definition (fold).
- **Storage backends** resolve via `[storage].backend` / `GRAPH_STORAGE` env (`file` default, `memory` for CI). The file store tolerates concurrent processes: whole files write via temp-file+rename, message appends are O_APPEND under a per-thread flock, shape writes are last-writer-wins (`seen_count` is advisory). Local filesystems only (flock over NFS is unreliable). The shape cache is read fresh at each planning attempt — never snapshot it at construction.
- **Model roles**: `chat`, `planner`, `solver`, `repair`, `judge` (+ `embedder` reserved), all falling back to `default`. Structured output is provider-native (forced tool on Anthropic, json_schema with json_object fallback on OpenAI-compat) with one `repair`-role fix-up pass.
- **Execution gate + step events**: every real tool call at any depth funnels through the private `Pipeline::dispatch`; an optional `ExecutionGate` (Proceed/Skip{injected result}/Abort) is consulted there — never for control-step evaluation. `GateContext` carries the rendering scope (results map / layered body scope with pseudo-roots) — the debugger's locals. `on_tool_error` (defaulted → Fail) is the break-on-exception hook: Replace substitutes a value and continues (never replans); `tool_finished` reports the real call, `step_finished` the resolution. Aborts bypass replan/solver/error-summary and surface as `PipelineError::Aborted{state}` (nested plans propagate via `PlanCall.aborted`; nested aborts are never re-asked as errors). `EventSink::step_started/step_finished` carry rendered inputs and full results with bus-syntax paths (`E3/do.2/E10`) + the plan call stack. `Pipeline::draft_plan` is the plan-without-executing planner entry (revisions go in a dedicated "Draft Under Revision" prompt section, NOT `existing_plan` — that slot means executed-and-immutable). Workbench debugger state (breakpoints/continue) lives UI-side in `DebugControls`; the oneshot reply is a `UiDecision` so continue-mode lands before the engine resumes.

## Invariants to preserve

- **Error policy**: human-authored plans never replan; `EmptyData` (data ran out) is distinct from `BadPath` (plan defect) and never triggers replanning; output/silent plans fail hard on empty data — `exit` gates are the sanctioned way to pre-empt that. See `docs/plans/errors-and-replanning.mdx`.
- **Template dialect** is strict, typed, and logic-less; its contract lives in `graph-core/src/template/` tests and `docs/plans/template-language.mdx`. Changes to one must change both. Logic belongs in steps (user tools, exit conditions), never in templates.
- **Streams contract**: stdout carries only the deliverable (answer, output-mode JSON, `--json` envelopes); all progress/diagnostics go to stderr (`GRAPH_EVENTS=jsonl` switches stderr to machine-parseable events). Exit codes: 0 ok, 1 failure, 3 needs-input, 4 exit-gate assertion. One sanctioned exception: `GRAPH_EVENTS=github` prints a `::error::` annotation to stdout on *failing* `plan run`s (GitHub parses workflow commands from stdout only) — the choke point is `output::annotate_failure`.
- **MCP child processes** must be shut down via `McpManager::shutdown()` before the tokio runtime drops (async-Drop cleanup doesn't run during teardown; orphans attach to the user's terminal).
- **Store access** goes through `Arc<dyn Store>`; commands never touch a concrete backend type directly.
- Prompts are prompt *surface*: planner-facing field names (`toolName`, `queryToAnswer`) are camelCase via serde and must stay aligned with `graph-core/src/pipeline/prompts.rs`; tool descriptions and exemplars are routing signals, not comments.
- **Never put link flags in env-wide RUSTFLAGS** — they poison build scripts (segfaulted x86_64 CI). Cargo quirk to remember: `rustc-link-lib` from a build script reaches downstream binaries but not the emitting package's own tests (those need `rustc-link-arg-tests`).

## Conventions

- Conventional commits (`feat:`/`fix:`/`docs:`/`chore:`/`ci:`) — the changelog is generated from them; subjects describe the user-visible effect. See RELEASING.md.
- Behavioral tests colocated per crate; mock LLM providers via `ModelRouter::with_providers`; scripted-response mocks for pipeline/agent tests.
- Every feature lands with tests and a lint-clean tree, then gets **live verification**: the MCP reference server (`npx @modelcontextprotocol/server-everything`) for tool mechanics, the user's real Linear workspace for plan behavior. Use the scratch pattern — a temp dir with `.graph/config.toml` overriding `data_dir` (and `GRAPH_STORAGE=memory`) — to keep the user's real state out of test runs.
- Linux behavior is reproducible locally in a container (`podman run --rm -v $PWD:/repo:ro rust:1.94-trixie …`).
- Live example plans and user tools ship in the user's `~/.config/graph/{plans,tools}/` (sprint_analysis, project_status, urgent_issues; git_log, summarize) — useful as behavioral references.

## Worktrees

Always use worktrees for any coding work. Never make changes directly on the main branch. When spawning subagents, set `isolation: worktree` in their frontmatter.
