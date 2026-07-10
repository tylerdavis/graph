# graph

A plan engine for repeatable, AI-augmented workflows.

`graph` runs multi-step plans where the steps are deterministic and
inference happens only where you put it. Author a plan once, review it,
and run it forever — in your terminal or in CI.

- **Plan-based execution engine** — validated multi-step plans with
  `{{Ex.path}}` dataflow between steps and inference only where placed
  (prompt-tool steps, a solver finish). Deterministic, reviewable, 0–1 LLM
  calls per run. The LLM planner (`plan_and_execute`) authors validated
  plans on the fly and replans against real errors; each user-authored plan
  document is exposed as a tool of its own, so plans compose.
- **Exit gates** — declarative `when`/`infer` conditions that short-circuit
  a plan (success or failure) before wasting steps, with CI-friendly exit
  codes.
- **MCP tools** — connect any Model Context Protocol server (stdio or HTTP).
- **User-defined tools** — YAML definitions for exec/shell, Cypher, and
  prompt tools, plus bundled `builtin__` tool packs.
- **Embedded graph database** — [LadybugDB](https://ladybugdb.com/) stores
  threads, run history, tool shape knowledge, and your entity graph.
- **Bring your own models** — Anthropic, OpenAI, OpenAI-compatible (local),
  and AWS Bedrock, assignable per role (chat, planner, solver, judge, …).
- **Chat workbench** — `graph ask` and `graph chat` run a tool-calling agent
  loop for probing tools and prototyping workflows before freezing them into
  plans.

## Workspace

| Crate | Contents |
|---|---|
| `graph-cli` | binary: command tree, REPL, output UX |
| `graph-core` | agent loop, plan pipeline, template renderer, prompts |
| `graph-llm` | provider abstraction (tool use, structured output, streaming) |
| `graph-mcp` | MCP client manager and tool adaptation |
| `graph-store` | LadybugDB storage and built-in graph tools |
| `graph-config` | layered TOML config + YAML plan/tool documents |

## Building (macOS)

Tooling (Rust, cmake, pkg-config) and tasks are managed with
[mise](https://mise.jdx.dev). OpenSSL can come from anywhere (nix, Homebrew,
…) as long as `pkg-config` can resolve it:

```nu
mise trust
mise install
mise run build
```

Common tasks: `mise run test`, `mise run test:spike`, `mise run lint`,
`mise run run -- config show`.

OpenSSL (from nix, Homebrew, or apt) must be resolvable via `pkg-config`;
the workspace's build scripts handle the embedded database's linker quirks
(see `crates/graph-store/SPIKE.md`). Plain `cargo build` works.

## Documentation

The `docs/` directory is the canonical reference for plan format, template
language, tool definitions, and CLI surface. Start with
`docs/getting-started/concepts.mdx`.

## License

MIT — see [LICENSE](LICENSE).

## Special thanks

To the people whose ideas and code shaped graph's design:
[@salscode](https://github.com/salscode),
[@mrhamric](https://github.com/mrhamric),
[@gjdame](https://github.com/gjdame),
[@bryceholcomb](https://github.com/bryceholcomb),
[@Korywon](https://github.com/Korywon),
[@brianhasenstab](https://github.com/brianhasenstab), and
[@ashleyhomen](https://github.com/ashleyhomen).
