---
name: use-graph
description: General entry point for working with graph. Use when running or
  inspecting plans, probing the tool catalog, reading a run's exit code or
  output, prototyping with ask/chat, or deciding which deeper graph skill
  (project setup, plan authoring, GitHub Actions setup) the task actually
  needs.
---

# Using graph

graph is a CLI agent whose core product is **plans**: YAML documents of tool
calls chained by a typed, logic-less template language, executed
deterministically with inference only where the plan puts it. Everything else —
the `ask`/`chat` agent loop, the workbench TUI, the MCP surfaces — exists to
build, exercise, and review plans.

This skill is the map. It covers the everyday surface (running plans,
inspecting the catalog, reading results) directly, and routes anything deeper
to the skill or doc that owns it. Load the deeper material only when the task
reaches it.

## Route first

| The task is… | Go to |
|---|---|
| Installing graph, scaffolding `./.graph/`, wiring a provider or MCP servers, landing a first plan | [`/graph-project-setup`](https://github.com/tylerdavis/graph/blob/main/skills/graph-project-setup/SKILL.md) |
| Creating a new plan, or editing/repairing/extending an existing one | [`/graph-plan-authoring`](https://github.com/tylerdavis/graph/blob/main/skills/graph-plan-authoring/SKILL.md) |
| Putting graph in CI — merge gates, PR reviewers, GitHub Actions | [`/graph-github-actions-setup`](https://github.com/tylerdavis/graph/blob/main/skills/graph-github-actions-setup/SKILL.md) |
| Running, inspecting, probing, debugging what already exists | stay here |

The [docs site](https://github.com/tylerdavis/graph/tree/main/docs) is the
canonical reference and moves ahead of every skill; when a skill and the docs
disagree, the docs win. Start with
[`getting-started/concepts.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/getting-started/concepts.mdx)
if the mental model below isn't enough.

## The mental model (60 seconds)

- **A plan is steps; steps are tool calls.** `{{E0.teams.0.name}}` pipes one
  step's result into the next — dataflow, zero inference. Control flow is a
  small set of executor-intercepted steps (`exit`, `decide`, `filter`,
  `map`/`reduce`, `agent`, `ask`). How a plan ends is its
  [finish mode](https://github.com/tylerdavis/graph/blob/main/docs/plans/finish-modes.mdx):
  a solver **report** (one LLM call), structured **output** JSON (zero), or
  **silent** side effects (zero).
- **One tool catalog.** MCP servers (`linear__*`), user tools (`user__*`),
  bundled packs (`builtin__*`), and plans themselves (`plan__*`) all live in
  one namespaced catalog. Authoring a plan adds a tool; plans compose.
- **The shape cache makes templates possible.** Every successful tool call
  records what that tool actually returned. Probing tools is never wasted —
  it is what grounds the next plan's `{{…}}` paths.
- **Human-authored plans never replan.** A failing step is a structured error,
  not an excuse to improvise. Only `plan_and_execute` (LLM-authored, on the
  fly) replans.
- **Streams are disciplined.** stdout carries only the deliverable; progress
  and diagnostics go to stderr. Exit codes: `0` ok, `1` failure, `3` needs
  input (schema printed), `4` an exit gate fired deliberately. Scripts read
  the exit code and `--json` envelopes, never scrape prose.

## Running plans

```bash
graph plan list                                  # the runnable catalog
graph plan show <name> [--json]                  # the YAML / full structure
graph plan validate <name> [--json]              # static check; exit code is the verdict
graph plan run <name> '<json>' [--json]          # run against real input
graph plan run <name> --input base=v0.8.0 --input head=main
```

- Input is one JSON object — inline, `@file`, or `-` for stdin — or repeated
  `--input k=v` pairs.
- **Read the exit code first.** `3` means the input schema (printed to stderr)
  wasn't satisfied; `4` means an `exit` gate fired on purpose — read its
  message, it is the plan speaking; `1` means the plan itself broke — that is
  a defect to fix, not a result to accept.
- `list` shows only what is runnable here; `list --json` also reports plans
  `skipped` (failed to load) and `hidden` (missing MCP servers). `show` and
  `validate` deliberately open broken or hidden plans — the plan you cannot
  run is exactly the one you need to inspect.
- A validation `note` (e.g. an unconfigured server) is not a failure — the
  plan is portable, just not runnable on this machine.

Where plans come from: the repo-local `./.graph/plans/` and the user-global
`~/.config/graph/plans/`, per the
[configuration reference](https://github.com/tylerdavis/graph/blob/main/docs/reference/configuration.mdx).

## Probing and prototyping: `ask` / `chat`

```bash
graph ask "which linear issues are stuck in review?"
graph ask "and who is assigned?" --thread        # continue the latest thread
graph chat                                       # interactive REPL
```

The agent loop is a standard tool-calling loop over the same catalog — plans
included, called like any other tool. Use it to probe unfamiliar tools, sketch
a workflow conversationally, and seed the shape cache; **freeze anything you
would run twice into a plan** (that is the `/graph-plan-authoring` handoff).
Turns persist as threads: `graph threads list / show <id> / rm <id>`.

## Inspecting the catalog

```bash
graph tools list [--json]            # every tool, grouped by source
graph tools show <name> --json       # one tool's full definition + input schema
graph tools test <name> '<json>'     # call it for real, see what comes back
graph shapes show <tool>             # what it has returned before (the shape cache)
graph mcp list / test <server>       # configured MCP servers, per-server connectivity
```

`tools test` is the ground truth for "what does this tool return" — use it
instead of guessing at a field name, and note it feeds the shape cache. Test
MCP servers individually (`graph mcp test <name>`); a full-catalog command
like `tools list` enumerates *every* server, so one broken server block makes
the whole catalog look broken.

## Creating and editing plans — the short version

The full lifecycle belongs to
[`/graph-plan-authoring`](https://github.com/tylerdavis/graph/blob/main/skills/graph-plan-authoring/SKILL.md);
the three rules worth knowing before you load it:

1. **Draft first, by command.** `graph plan draft "<goal>"` is the starting
   point for any new plan — one round of inference, returns a statically
   valid plan grounded in the real catalog. Don't assemble steps by hand.
2. **Then edit, never redraft.** Corrections go through `graph plan set` /
   `graph plan step add|update|rename|rm` — each edit is validated atomically
   and refused if it would break the plan. Redrafting replaces every step.
3. **Never hand-edit the YAML.** A text editor bypasses validation, and every
   command edit reserializes the file anyway. `graph plan validate` is the
   verdict; `graph plan run` is the proof.

## Plans are composable units

Authoring a plan adds `plan__<identifier>` to the catalog, and plan steps can
call anything in the catalog — including other plans. Composition is bounded
and safe: the pipeline names cycles immediately and caps nesting at 8 levels
([composition rules](https://github.com/tylerdavis/graph/blob/main/docs/tools/overview.mdx)).

So don't build monoliths. **Break a plan apart when a piece is reusable on
its own, or when the split makes organizational sense** — a sub-plan with a
clear name, description, and input schema is easier to validate, test
(`graph plan run` it directly), and reason about than the same steps inlined,
and every caller gets it for free: other plans, `ask`/`chat`, the planner,
and MCP clients all see the same `plan__*` tool. Composition is also the
sanctioned way to put multi-step work inside a `map`/`reduce` body or a
`decide` branch — bodies can't nest control steps, so factor the work into
its own plan and call it per item.

## The workbench

`graph wb plan [<name>]` opens the plan workbench — the dual-pane TUI for
reviewing and debugging plans. A chat agent drafts and edits in one pane while
you inspect steps, validate on every change, and run under a debugger that
pauses before each tool call — the way to trust a plan's writes before
committing it. See
[`workbench/plan-workbench.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/workbench/plan-workbench.mdx).
(The workbench is this TUI; `ask`/`chat` is the agent loop, not the
workbench.)

## graph over MCP

Both directions, one catalog:

- **Client:** each `[mcp.<name>]` config block contributes `<name>__*` tools.
- **Server:** `graph mcp serve` exposes every plan as a callable tool plus the
  authoring commands — so another agent (including this one) can build and
  run plans over MCP instead of shelling out. If those tools are connected,
  prefer them; same commands, structured results.

## Reference — read only when needed

- Full command surface → [`reference/cli.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/cli.mdx)
- `--json` envelopes, streams, exit codes → [`reference/scripting-contract.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/scripting-contract.mdx)
- Every plan field → [`reference/plan-schema.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/plan-schema.mdx)
- Config layers, data dir, model roles → [`reference/configuration.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/configuration.mdx)
- Why a run failed / error taxonomy → [`plans/errors-and-replanning.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/errors-and-replanning.mdx)
