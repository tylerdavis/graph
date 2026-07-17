---
name: graph-project-setup
description: Set up graph locally in a project and land a first working plan. Use
  when installing graph for local use, scaffolding a ./.graph/ directory, wiring
  a provider and models, connecting MCP servers or tools, or authoring and
  running a first plan against a user's own data.
---

# Graph Project Setup

The [graph repository](https://github.com/tylerdavis/graph) is the source of truth. Before generating anything, read the current onboarding docs — they move ahead of this skill; prefer them over memory:

- [`docs/getting-started/quickstart.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/getting-started/quickstart.mdx) — the install → scaffold → run arc this skill drives.
- [`docs/getting-started/concepts.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/getting-started/concepts.mdx) — plans, the tool catalog, the shape cache.
- [`docs/plans/authoring.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/authoring.mdx) — builds `project_status`, the plan to adapt for the first one.
- [`docs/plans/finish-modes.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/finish-modes.mdx), [`docs/tools/mcp-servers.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/tools/mcp-servers.mdx), [`docs/tools/user-defined.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/tools/user-defined.mdx).

The finished shape: a `./.graph/` directory carrying `config.toml` (a provider, models, and the tools the plan needs) and one plan under `./.graph/plans/` that runs end to end against the user's real data.

**This is an interactive, question-driven setup.** Ask the questions below one topic at a time, present real choices, and let each answer drive what you scaffold next. Do not scaffold everything up front and ask for confirmation at the end.

## Interactive Workflow

### Step 0: Preflight

1. **Confirm graph is installed** — `graph --help`. If it fails, point at [installation](https://github.com/tylerdavis/graph/blob/main/docs/getting-started/installation.mdx) and stop here.
2. **Detect an existing `./.graph/`** — if one exists, don't clobber it. Read the current `config.toml` and plans, and extend rather than overwrite.
3. **Confirm a provider key is reachable** in the environment (e.g. `ANTHROPIC_API_KEY`). Surface this now — the user needs it before the first plan runs, so flag a missing key early rather than at the end.

### Step 1: What should the first plan do?

Ask for **one concrete outcome** — the smallest useful thing worth running twice. Prompt with candidates:

- a **status report** (a project, a sprint, a service) rendered as prose;
- a **data pull / ETL** that emits structured JSON for another tool;
- a **triage / classification** pass over a list (issues, alerts, messages).

This answer is the spine. Everything after it — provider, tools, finish mode — is scaffolding toward this one plan. Keep it small; the user can grow it later.

### Step 2: Provider and models

Ask which provider to use: **Anthropic**, **OpenAI**, or an **OpenAI-compatible / local** endpoint (Ollama, vLLM, a gateway). Write `[providers.*]` and `[models] default` accordingly, secrets always as `${ENV}` references:

```toml
[providers.anthropic]
type = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"   # read from the environment at load time

[models]
default = { provider = "anthropic", model = "claude-sonnet-5" }
```

If the plan from Step 1 will use an `infer` gate or a `decide` step, add the cheap roles — those are single yes/no calls and belong on a fast model:

```toml
judge  = { provider = "anthropic", model = "claude-haiku-4-5" }
repair = { provider = "anthropic", model = "claude-haiku-4-5" }
```

Run `graph config init` first if `./.graph/config.toml` doesn't exist yet (it targets `./.graph/` by default), then edit it — don't hand-write the whole file from scratch.

### Step 3: What data does the plan touch? — connect the tools

This is the interactive heart of the setup. graph reaches external systems through **MCP servers**; it also ships [bundled tool packs](https://github.com/tylerdavis/graph/blob/main/docs/tools/builtins.mdx) and supports [user-defined tools](https://github.com/tylerdavis/graph/blob/main/docs/tools/user-defined.mdx) that wrap a CLI or query.

1. **Ask what systems/data the plan needs** — free-form. E.g. "our Linear issues", "a Postgres orders table", "files in this repo", "Slack messages".

2. **Web-search for matching MCP servers** based on that answer (e.g. search "Linear MCP server", "Postgres MCP server"). Do not rely on a hardcoded list — find current, real servers. For each candidate, confirm from **its own README/docs** (not from search-snippet memory) three things:
   - **transport** — remote/HTTP (has a `url`) or local/stdio (has a launch `command`);
   - **auth** — what token/key it needs, and the env var name;
   - the exact **config-block shape** it expects.

   Search results are unverified. Never paste a guessed URL or command — open the server's docs and copy the real shape.

3. **Present a multi-select** of the servers you found: name, one-line purpose, transport, and the key it requires. Include an explicit **"none — use a bundled pack or a user tool instead"** option so an ETL/CLI plan isn't forced through MCP. Let the user pick any subset.

4. **Wire each chosen server** into `./.graph/config.toml` as an `[mcp.<name>]` block, secrets as `${ENV}` references — **never** a live token inline (that's graph's fail-loud contract). Then test **by transport type**:

   **Remote / HTTP** (`url` + auth header):
   ```toml
   [mcp.linear]
   url = "https://mcp.linear.app/mcp"
   headers = { Authorization = "Bearer ${LINEAR_API_KEY}" }
   ```
   Tell the user which env var to export and where to get the key, then run `graph mcp test linear`.

   **Stdio via `npx` / `uvx`** (zero pre-install — the launcher fetches the server on first run):
   ```toml
   [mcp.filesystem]
   command = "npx"
   args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
   ```
   The first `graph mcp test <name>` **executes this command**, which fetches and runs a package off the network. **Show the literal command line and ask permission before running the test.** Do not run an unpinned network fetch silently.

   **Stdio needing a real install** (a binary, a `pip`/`cargo`/`go install`, a cloned repo):
   Write the `[mcp.<name>]` block with the intended `command`, then **hand the user the server's own install instructions** and ask them to run `graph mcp test <name>` once it's on PATH. Do not attempt the install yourself.

5. **Test each server individually by name** — `graph mcp test <name>`. Per-server tests are isolated: a server that isn't installed yet fails only its own test and does **not** block the others. Config for an un-installed server is inert until something connects to it, so leaving its block in place (with install instructions handed off) is safe — the user finishes the install later and re-runs just that one test.

6. **Confirm the catalog last** — `graph tools list`. Run this **after** the pending servers are either installed or explicitly noted as expected-to-fail: unlike per-server tests, `graph tools list` enumerates *every* server, so one un-installed stdio block will surface as an error there and make the whole catalog look broken. This step gives you the real tool names the next step builds `{{...}}` paths from.

**graph connects to MCP servers; it does not install them.** For `npx`/`uvx` servers the launcher handles fetching on first run; everything else is the user's install to run. Say so — never claim graph installs a server.

### Step 4: How should the plan finish?

Map the Step 1 outcome to a [finish mode](https://github.com/tylerdavis/graph/blob/main/docs/plans/finish-modes.mdx):

- **report** — the solver writes prose from the collected step results (status reports);
- **output** — structured JSON for another tool to consume (ETL, data pulls);
- **silent** — no deliverable, side effects only (a plan that posts a comment or writes a file).

This sets the plan's shape and what a run costs (a report is one solver call; output/silent are zero).

### Step 5: Where will it run?

Ask: just locally, or also on cron / in CI later?

- **Local only** — nothing more to do; state lives under the data directory as plain files.
- **Cron / CI** — keep storage ephemeral there (`GRAPH_STORAGE=memory`), and for GitHub Actions hand off to the [`/graph-github-actions-setup`](https://github.com/tylerdavis/graph/blob/main/skills/graph-github-actions-setup/SKILL.md) skill, which scaffolds the workflow and secrets.

### Step 6: Scaffold and run the first plan

1. **Seed the shape cache** — before authoring, probe the chosen tools so the plan's `{{...}}` paths are grounded in real output, not guesses:
   ```bash
   graph ask "<a question that exercises the tools from Step 3>"
   ```
   Every call records what each tool actually returns (the [shape cache](https://github.com/tylerdavis/graph/blob/main/docs/tools/shape-cache.mdx)), which is exactly what the plan's template paths reference.
2. **Author one plan** under `./.graph/plans/`, adapted from [`project_status`](https://github.com/tylerdavis/graph/blob/main/docs/plans/authoring.mdx) — the same probe → chain steps → shape the finish workflow — matching the finish mode from Step 4.
3. **Validate, then run** against real input:
   ```bash
   graph plan validate <name>
   graph plan run <name> '{"…":"…"}'
   ```
   Interpret the exit code: **0** ok, **3** needs input (a required field was missing), **4** an exit gate fired, **1** the plan itself is broken — fix that before calling it done.

### Step 7: Hand off

- Point to [authoring plans](https://github.com/tylerdavis/graph/blob/main/docs/plans/authoring.mdx) to extend the plan and [user-defined tools](https://github.com/tylerdavis/graph/blob/main/docs/tools/user-defined.mdx) to wrap a CLI or an inline LLM prompt.
- If Step 5 was CI, hand off to [`/graph-github-actions-setup`](https://github.com/tylerdavis/graph/blob/main/skills/graph-github-actions-setup/SKILL.md).

## Key Concepts

**graph connects, it doesn't install.** An `[mcp.<name>]` block is a connection, not an installation. `npx`/`uvx` servers self-fetch on first launch; anything else the user installs. A block for an un-installed server is inert until connected to.

**Per-server tests are isolated; the catalog is not.** `graph mcp test <name>` touches only that server — a broken block fails only its own test. `graph tools list` and any full-catalog load (a `graph ask`, a plan run) enumerate every server, so a pending stdio block surfaces there. Test individually while setting up; confirm the whole catalog last.

**Secrets are `${ENV}` references, always.** They fail loudly when the variable is missing — a misconfigured secret never silently sends an empty string. Never write a live token into `config.toml`.

**Never fetch-and-run silently.** The first test of an `npx`/`uvx` stdio server executes a network fetch. Show the command and get permission first.

**Probe before you author.** The shape cache is what lets the plan reference `{{E0.teams.0.name}}` correctly. A few `graph ask` turns over the real tools make the first plan run on the first try.

### Checklist

- [ ] `./.graph/config.toml` has a provider, `[models] default`, and secrets as `${ENV}` references
- [ ] Each chosen MCP server has an `[mcp.<name>]` block and passes `graph mcp test <name>` (or has install instructions handed to the user)
- [ ] Permission requested before the first `npx`/`uvx` server test
- [ ] `graph tools list` run last, after pending installs — shows the real tool catalog
- [ ] Tools probed with `graph ask` to seed the shape cache before authoring
- [ ] One plan under `./.graph/plans/`, matching the chosen finish mode, passes `graph plan validate`
- [ ] `graph plan run <name>` returns exit 0 against real input
