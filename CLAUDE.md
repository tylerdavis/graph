# graph-cli

A Rust CLI agent with a plan-based execution engine. Cargo workspace, six crates.

**The documentation under `docs/` is the canonical reference** for behavior, file formats, and CLI surface — read it before changing the corresponding code, and update it in the same PR as any behavior change. Start with `docs/getting-started/concepts.mdx` and `docs/architecture/execution-model.mdx`. It is published via Mintlify (docs.json is the nav manifest).

## Build & test

Tooling is managed by mise; the linker flags for the embedded database live in mise's env — plain `cargo` fails at link time outside a mise-activated shell.

```bash
mise run build          # cargo build
mise run test           # all workspace tests
mise run lint           # fmt --check + clippy -D warnings (run before committing)
mise run run -- <args>  # run the CLI
mise run install        # release build onto PATH
```

## Crate map

| Crate | Contents |
|---|---|
| `graph-cli` | binary: clap tree, REPL, output sink (`TtySink`), runtime wiring |
| `graph-core` | agent loop, plan pipeline, template engine, toolbox, user tools, Store/ToolRegistry traits, prompts |
| `graph-llm` | ChatProvider trait + anthropic/openai_compat providers, structured output + repair, ModelRouter (role → model) |
| `graph-mcp` | rmcp client manager (stdio + streamable HTTP), `server__tool` namespacing |
| `graph-store` | LadybugDB Store impl, MemoryStore, RecordingRegistry (shape cache), CypherExecutor |
| `graph-config` | layered TOML config + serde models |

## Invariants to preserve

- **Error policy**: human-authored plans never replan; `EmptyData` (data ran out) is distinct from `BadPath` (plan defect) and never triggers replanning; output/silent plans fail hard on empty data. See `docs/plans/errors-and-replanning.mdx`.
- **Template dialect** is strict and logic-less; its contract lives in `graph-core/src/template/` tests and `docs/plans/template-language.mdx`. Changes to one must change both.
- **Streams**: stdout carries only the deliverable; all progress/diagnostics go to stderr.
- **MCP child processes** must be shut down via `McpManager::shutdown()` before the tokio runtime drops (async-Drop cleanup doesn't run during teardown; orphans attach to the user's terminal).
- **Store access** goes through `Arc<dyn Store>`; only `graph db query` and `CypherExecutor` may touch `GraphStore` concretely.
- Prompts are prompt *surface*: planner-facing field names (`toolName`, `queryToAnswer`) are camelCase via serde and must stay aligned with `graph-core/src/pipeline/prompts.rs`.

## Conventions

- Vitest-style behavioral tests colocated per crate; mock LLM providers via `ModelRouter::with_providers`.
- Every feature lands with tests and a lint-clean tree; verify real behavior with the live MCP reference server (`npx @modelcontextprotocol/server-everything`) where practical.
- lbug 0.18 workarounds (OpenSSL link flags, `-export_dynamic`) are documented in `crates/graph-store/SPIKE.md` — drop them when lbug > 0.18.0 ships.
