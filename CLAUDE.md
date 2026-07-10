# graph-cli

A Rust CLI agent with a plan-based execution engine. Cargo workspace, six crates. GitHub: `tylerdavis/graph`.

**Positioning: plans first.** The plan pipeline is the core product; the agent loop (`ask`/`chat`) is the workbench layered over it — for probing tools, prototyping, and exercising plans conversationally. Frame features and docs accordingly.

**The documentation under `docs/` is the canonical reference** for behavior, file formats, and CLI surface — read it before changing the corresponding code, and update it in the same PR as any behavior change. Start with `docs/getting-started/concepts.mdx` and `docs/architecture/execution-model.mdx`. Published via Mintlify from this repo (`docs.json` is the nav manifest; monorepo contentDirectory `docs/`; builds only trigger when `docs/` changes; the site is private).

## Build & test

Tooling is managed by mise. The embedded database's linker quirks are handled by build scripts (`crates/graph-store/build.rs`, `crates/graph-cli/build.rs`); plain `cargo` works, including rust-analyzer.

```bash
mise run build          # cargo build
mise run test           # all workspace tests
mise run lint           # fmt --check + clippy -D warnings (run before committing)
mise run run -- <args>  # run the CLI
mise run install        # release build onto PATH
mise run release:patch  # cut a release (also :minor / :major) — see RELEASING.md
```

CI (`.github/workflows/ci.yaml`) runs lint + tests on ubuntu-24.04 and macos-15 for every push/PR touching non-docs paths; sccache (incl. `CMAKE_*_COMPILER_LAUNCHER` for lbug's C++) keeps cache-key rotations from recompiling the world. Tags `v*` trigger `release.yaml`: binaries for macOS arm64 + Linux x86_64 with checksums, plus the `ghcr.io/tylerdavis/graph` container image (graph + git/jq/gh; `workflow_dispatch` with a `tag` input re-mints an image for an existing release).

**graph dogfoods itself on every PR** (`.github/workflows/graph-checks.yaml`): the `docs_drift` plan fails the check (exit 4) on undocumented crate behavior changes, and `pr_review` posts rubric'd findings as a PR comment — both run from the repo-carried `./.graph/` inside the release image (needs the `ANTHROPIC_API_KEY` repo secret). `.graph/` is partially tracked: config, plans, and tools are committed; everything else under it stays ignored. Expect the drift gate to hold you to the docs-parity invariant below.

## Crate map

| Crate | Contents |
|---|---|
| `graph-cli` | binary: clap tree, REPL, sinks (`TtySink`, `JsonlSink`), input resolution, runtime wiring |
| `graph-core` | agent loop, plan pipeline (incl. `exit` gates, plan composition), template engine, toolbox, user tools + bundled packs (`src/packs/`), Store/ToolRegistry traits, prompts, shape inference |
| `graph-llm` | ChatProvider trait + anthropic/openai_compat providers, retries with backoff, structured output + repair, ModelRouter (role → model) |
| `graph-mcp` | rmcp client manager (stdio + streamable HTTP), `server__tool` namespacing, graceful shutdown |
| `graph-store` | LadybugDB Store impl, MemoryStore (ephemeral/CI), RecordingRegistry (shape cache), CypherExecutor |
| `graph-config` | layered TOML config + serde models (providers, model roles incl. `judge`, storage backends, plan/tool paths, tool packs) |

## Architecture facts that bite

- **Tool catalog namespacing**: `<server>__` (MCP), `user__` (user tools), `builtin__` (bundled packs), `plan__` (plan docs), plus reserved bare names `plan_and_execute` and `exit`. Everything shares one `ToolRegistry` catalog (`CompositeRegistry` merges sources); shape recording wraps the base. Packs (`[tools].packs`, YAML embedded in `graph-core/src/packs/`) reuse the user-tool format and registry with a different prefix; customizing one means copying it into a tools dir as a `user__` tool.
- **Plan invocation lives in the pipeline**, not the toolbox: `Pipeline::call_plan`/`call_planner` (boxed recursive futures). The toolbox is a thin adapter for the agent loop. **Plans compose**: `plan__*` and `plan_and_execute` are valid step tools; the pipeline call stack detects cycles and caps depth at 8.
- **`exit` gates** are executor-intercepted steps (never dispatched to a registry): `when` (logical value/op/to) or `infer` (yes/no verdict via the `judge` role). Fired gates skip remaining steps *and the solver*; error exits → process exit code 4, `is_error` plan-tool results.
- **Storage backends** resolve via `[storage].backend` / `GRAPH_STORAGE` env (`ladybug` default, `memory` for CI). Ladybug is single-process (lock errors name the holding PID). The shape cache is read fresh at each planning attempt — never snapshot it at construction.
- **Model roles**: `chat`, `planner`, `solver`, `repair`, `judge` (+ `embedder` reserved), all falling back to `default`. Structured output is provider-native (forced tool on Anthropic, json_schema with json_object fallback on OpenAI-compat) with one `repair`-role fix-up pass.

## Invariants to preserve

- **Error policy**: human-authored plans never replan; `EmptyData` (data ran out) is distinct from `BadPath` (plan defect) and never triggers replanning; output/silent plans fail hard on empty data — `exit` gates are the sanctioned way to pre-empt that. See `docs/plans/errors-and-replanning.mdx`.
- **Template dialect** is strict, typed, and logic-less; its contract lives in `graph-core/src/template/` tests and `docs/plans/template-language.mdx`. Changes to one must change both. Logic belongs in steps (user tools, exit conditions), never in templates.
- **Streams contract**: stdout carries only the deliverable (answer, output-mode JSON, `--json` envelopes); all progress/diagnostics go to stderr (`GRAPH_EVENTS=jsonl` switches stderr to machine-parseable events). Exit codes: 0 ok, 1 failure, 3 needs-input, 4 exit-gate assertion. One sanctioned exception: `GRAPH_EVENTS=github` prints a `::error::` annotation to stdout on *failing* `plan run`s (GitHub parses workflow commands from stdout only) — the choke point is `output::annotate_failure`.
- **MCP child processes** must be shut down via `McpManager::shutdown()` before the tokio runtime drops (async-Drop cleanup doesn't run during teardown; orphans attach to the user's terminal).
- **Store access** goes through `Arc<dyn Store>`; only `graph db query` and `CypherExecutor` may touch `GraphStore` concretely.
- Prompts are prompt *surface*: planner-facing field names (`toolName`, `queryToAnswer`) are camelCase via serde and must stay aligned with `graph-core/src/pipeline/prompts.rs`; tool descriptions and exemplars are routing signals, not comments.
- **Never put link flags in env-wide RUSTFLAGS** — they poison build scripts (segfaulted x86_64 CI). The lbug linker story lives in the two build.rs files and `crates/graph-store/SPIKE.md`; drop it all when lbug > 0.18.0 ships. Cargo quirk to remember: `rustc-link-lib` from a build script reaches downstream binaries but not the emitting package's own tests (those need `rustc-link-arg-tests`).
- **lbug extensions are vendored, never INSTALLed**: `graph-store/build.rs` fetches the fts/vector binaries (version pinned there — the engine requests v0.18.1 URLs despite the vendored CMake saying 0.18.0) and `src/extensions.rs` embeds them; runtime loads by file path from `<data_dir>/extensions/<ver>/`. `GRAPH_LBUG_EXT_DIR` (build-time env) points at pre-fetched files for offline builds. Tests must never use `INSTALL <ext>` — the CDN flakes.

## Conventions

- Conventional commits (`feat:`/`fix:`/`docs:`/`chore:`/`ci:`) — the changelog is generated from them; subjects describe the user-visible effect. See RELEASING.md.
- Behavioral tests colocated per crate; mock LLM providers via `ModelRouter::with_providers`; scripted-response mocks for pipeline/agent tests.
- Every feature lands with tests and a lint-clean tree, then gets **live verification**: the MCP reference server (`npx @modelcontextprotocol/server-everything`) for tool mechanics, the user's real Linear workspace for plan behavior. Use the scratch pattern — a temp dir with `.graph/config.toml` overriding `data_dir` (and `GRAPH_STORAGE=memory`) — to avoid the embedded DB lock and the user's real state.
- Linux behavior is reproducible locally in a container (`podman run --rm -v $PWD:/repo:ro rust:1.94-trixie …`); lbug needs GCC 13+ and real linker memory.
- Live example plans and user tools ship in the user's `~/.config/graph/{plans,tools}/` (sprint_analysis, project_status, urgent_issues; git_log, shape_cache, summarize) — useful as behavioral references.
