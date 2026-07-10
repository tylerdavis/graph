# graph

A command-line agent with a plan-based execution engine.

`graph` is a Rust re-implementation of the Nexus/Graph insight engine as a
standalone, single-user CLI:

- **ReAct agent front door** — `graph ask` and `graph chat` run a tool-calling
  agent loop over your configured tools.
- **Plan-based execution engine** — validated multi-step plans with
  `{{Ex.path}}` dataflow between steps, replanning on failure, and a solver
  that synthesizes results. Exposed to the agent as `plan_and_execute` and as
  one tool per user-authored plan document.
- **MCP tools** — connect any Model Context Protocol server (stdio or HTTP).
- **User-defined tools** — YAML definitions for exec/shell, Cypher, and
  prompt tools.
- **Embedded graph database** — [LadybugDB](https://ladybugdb.com/) stores
  threads, run history, tool shape knowledge, and your entity graph.
- **Bring your own models** — Anthropic, OpenAI, OpenAI-compatible (local),
  and AWS Bedrock, assignable per role (chat, planner, solver, …).

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

## Status

Early scaffold. See the plan in the originating repo for the roadmap.
