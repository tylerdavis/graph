---
name: graph-github-actions-setup
description: Set up graph as a GitHub Actions check on a repository. Use when
  installing graph in CI, adding an LLM-powered merge gate or PR reviewer,
  wiring graph plans into GitHub Actions, or scaffolding a repo-carried
  ./.graph/ directory for CI use.
---

# Graph GitHub Actions Setup

The [graph repository](https://github.com/tylerdavis/graph) is the source of truth — it dogfoods this exact setup on its own PRs. Before generating anything, read the worked example: [`docs/cookbook/ci-checks.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/cookbook/ci-checks.mdx) (annotated walkthrough), [`.github/workflows/graph-checks.yaml`](https://github.com/tylerdavis/graph/blob/main/.github/workflows/graph-checks.yaml) (live workflow), and [`.graph/`](https://github.com/tylerdavis/graph/tree/main/.graph) (live config, plans, and tools). Those move ahead of this skill; prefer them over memory.

The finished shape: a workflow job runs in graph's release container image (graph + `git`/`jq`/`gh` baked in — no install step), executes one `graph plan run` per check, and the repo carries everything the check needs under `./.graph/` (config, plans, tools), reviewed like any other code.

## Interactive Workflow

### Step 1: Preflight

1. **Detect the repo layout** — confirm it's a git repo hosted on GitHub. Look for existing `.github/workflows/` and an existing `./.graph/` directory (don't clobber one).
2. **Detect the default branch** — `git symbolic-ref refs/remotes/origin/HEAD`. Don't assume `main`.
3. **Resolve the image pin** — fetch the latest release tag: `gh release view --repo tylerdavis/graph --json tagName -q .tagName`. Pin the container image to that exact `vX.Y.Z` tag; never use `latest` in CI.
4. **Confirm an Anthropic API key exists** (or another supported provider). The user needs it as a repository secret in Step 5; ask now so nothing blocks at the end.
5. **Local graph install is optional but recommended** — it lets the user author and test plans before pushing (`graph plan run` locally). See [installation](https://github.com/tylerdavis/graph/blob/main/docs/getting-started/installation.mdx).

### Step 2: Decide the checks — gate or reporter?

Each check is one plan and one CI job. Every plan is one of two shapes, and picking wrong is the most common setup mistake:

**The test: should a bad answer block the merge button?**

- **Yes → a gate.** The plan ends in an [`exit` gate](https://github.com/tylerdavis/graph/blob/main/docs/plans/branching.mdx) with `status: error`. A fired gate is process exit code **4**, which fails the CI step; `GRAPH_EVENTS=github` prints the gate's message as a `::error::` run annotation. Examples: docs-drift checks, changelog-entry checks, "does this migration have a rollback" checks.
- **No → a reporter.** The plan posts its own PR comment (via the github tool pack) and exits 0. It's advisory; CI green either way. Examples: PR review with a repo-specific rubric, risk summaries, test-coverage narratives.

For each check the user wants, also apply the **short-circuit principle**: put deterministic `when` gates (file counts, empty diffs) *before* any `infer` gate or prompt step. A well-built gate exits in milliseconds with zero LLM calls on the common path and pays for at most one judge call on the rare path.

Ask, in order:

1. **What should CI enforce or report?** Prompt with candidates: a rule from their CLAUDE.md/CONTRIBUTING that's currently enforced by review comments, a PR-review rubric, a docs-must-accompany-behavior rule, release-note hygiene.
2. **For each: gate or reporter?** Apply the test above.
3. **For gates: what's the cheap deterministic pre-check?** (e.g. "only fires when files under `src/` changed").

### Step 3: Scaffold `./.graph/`

Run `graph config init` (targets `./.graph/` by default) or create the directory by hand. The repo carries three things, all committed: `config.toml`, `plans/`, `tools/`. Anything else graph writes under `.graph/` stays gitignored.

`config.toml` — secrets as `${ENV}` references, which fail loudly when the variable is missing (that's the fork-PR backstop, keep it):

```toml
[providers.anthropic]
type = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"

[models]
default = { provider = "anthropic", model = "claude-sonnet-5" }
# infer gates are single yes/no calls — route them to a fast model
judge   = { provider = "anthropic", model = "claude-haiku-4-5" }
repair  = { provider = "anthropic", model = "claude-haiku-4-5" }

# No [storage] section: CI sets GRAPH_STORAGE=memory.

[tools]
packs = ["github"]   # git_changed_files, git_diff, gh_pr_meta, gh_pr_comment, gh_pr_inline_comments
```

Then author one plan per check under `.graph/plans/`. Adapt from the live references — [`docs_drift.yaml`](https://github.com/tylerdavis/graph/blob/main/.graph/plans/docs_drift.yaml) (gate) and [`pr_review.yaml`](https://github.com/tylerdavis/graph/blob/main/.graph/plans/pr_review.yaml) (reporter) — rather than writing from scratch. A minimal gate skeleton:

```yaml
identifier: my_check
name: <human-readable name>
description: <what the check enforces and when it exits nonzero>
input_schema:
  type: object
  required: [base, head]
  properties:
    base: { type: string, description: Base ref or sha of the PR }
    head: { type: string, description: Head ref or sha of the PR }
steps:
  - id: E0
    tool_name: builtin__git_changed_files
    input: { base: "{{input.base}}", head: "{{input.head}}", prefix: "src/" }
  - id: E1                      # deterministic short-circuit: free, instant
    tool_name: exit
    input:
      when: { value: "{{E0.count}}", op: eq, to: 0 }
      status: success
      message: no src/ changes — check not applicable
  - id: E2
    tool_name: builtin__git_diff
    input: { base: "{{input.base}}", head: "{{input.head}}",
             paths: "src/", exclude: "", max_bytes: 60000 }
  - id: E3                      # the judged call, last resort
    tool_name: exit
    input:
      infer: |
        <A falsifiable yes/no question about the diff. Spell out what does
        NOT count — refactors, test-only changes — the framing sets the
        false-positive rate.>

        {{E2.text}}
      status: error
      message: <what the developer should do about it>
```

Verify plans load: `graph plan list` from the repo root.

### Step 4: Generate the workflow

Create `.github/workflows/graph-checks.yaml`, one job per check. Adapt this template (mirror of the [live workflow](https://github.com/tylerdavis/graph/blob/main/.github/workflows/graph-checks.yaml)):

```yaml
name: graph checks

on:
  pull_request:
    types: [opened, synchronize, reopened, ready_for_review]

permissions:
  contents: read
  pull-requests: write   # reporters only — drop for gate-only setups
  packages: read         # pull the release image while it's private

concurrency:
  group: graph-checks-${{ github.event.pull_request.number }}
  cancel-in-progress: true

env:
  GRAPH_STORAGE: memory          # ephemeral: leave no state behind in CI
  GRAPH_EVENTS: github           # failing gates annotate the run
  GH_TOKEN: ${{ github.token }}
  ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
  PR: ${{ github.event.pull_request.number }}
  BASE: ${{ github.event.pull_request.base.sha }}
  HEAD: ${{ github.event.pull_request.head.sha }}

jobs:
  my-check:
    # drafts pay nothing; fork PRs have no secrets (the ${VAR} config
    # reference hard-errors without the key — that's the backstop)
    if: >-
      !github.event.pull_request.draft &&
      github.event.pull_request.head.repo.full_name == github.repository
    runs-on: ubuntu-24.04
    container:
      image: ghcr.io/tylerdavis/graph:vX.Y.Z   # pin from Step 1
      credentials:
        username: ${{ github.actor }}
        password: ${{ secrets.GITHUB_TOKEN }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0        # REQUIRED — plans diff refs locally
      - run: graph plan run my_check --input base="$BASE" --input head="$HEAD"
```

Non-negotiables baked into the template:

- **`fetch-depth: 0`.** The git pack tools diff `$BASE...$HEAD` in the local clone. On a shallow clone the refs are missing and diffs come back *empty* — a gate then passes vacuously instead of erroring. This is the single most dangerous misconfiguration because it fails green.
- **Pinned image tag.** The image version is the graph version; upgrades are a reviewed one-line diff.
- **`GRAPH_STORAGE=memory`** — runs are stateless, nothing to clean up.
- **Draft + fork guards.** Drafts cost nothing (a `ready_for_review` trigger runs the check when the draft flips). Fork PRs are skipped because secrets don't flow to them.

For reporters that should post under their own name/avatar instead of **github-actions[bot]**, see [A custom bot identity](https://github.com/tylerdavis/graph/blob/main/docs/cookbook/ci-checks.mdx) — it's a GitHub App token swap on the one posting step, no graph changes.

### Step 5: Secrets

Add `ANTHROPIC_API_KEY` (or the provider key the config references): Repository Settings → Secrets and variables → Actions → New repository secret. While the graph image is private, the pulling account also needs `read:packages` on `ghcr.io/tylerdavis/graph` — the workflow's `GITHUB_TOKEN` covers this once the user has access to the graph repo.

### Step 6: Verify before merging

1. **Locally, if graph is installed** — run each plan against real refs with ephemeral storage:
   ```bash
   GRAPH_STORAGE=memory graph plan run my_check \
     --input base="$(git merge-base origin/main HEAD)" --input head=HEAD
   ```
   Exit 0 = passed/short-circuited, exit 4 = gate fired (read the message), exit 1 = the plan itself is broken — fix before shipping.
2. **Open a test PR** that should trip each gate (and one that shouldn't) and confirm the check fails with the annotation, then passes. A gate that has never fired is untested.

## Key Concepts

**Exit codes are the CI contract.** `0` success, `1` the plan itself failed, `3` needs input, `4` an exit gate fired (`status: error`). CI needs no output parsing or shell branching — a gate job is just `graph plan run …`, and the exit code does the rest. Treat exit 1 in CI as a bug in the plan, never as "check failed".

**Streams are disciplined.** stdout carries only the deliverable; all progress goes to stderr (`GRAPH_EVENTS=jsonl` for machine-parseable events). The one exception: `GRAPH_EVENTS=github` prints a `::error::` annotation to stdout on failing runs, because GitHub only parses workflow commands from stdout.

**The container image is the runtime.** `ghcr.io/tylerdavis/graph:vX.Y.Z` ships graph plus pinned `git`, `jq`, and `gh` (distro packages lag the flags the pack tools use) and the `safe.directory` git config Actions' workspace mounts require. No install step, no version drift between graph and its tool dependencies.

**Plans are code.** The repo-carried `.graph/` is reviewed, diffed, and versioned with everything else. Prompt wording in an `infer` gate or rubric tool is behavior — treat changes to it like changes to a linter config.

### Checklist

- [ ] `.graph/config.toml` references secrets as `${ENV}` (fails loudly when missing)
- [ ] `graph plan list` shows every plan the workflow calls
- [ ] `fetch-depth: 0` on checkout (shallow clones make gates pass vacuously)
- [ ] Image pinned to an exact `vX.Y.Z` release tag
- [ ] `GRAPH_STORAGE=memory` and `GRAPH_EVENTS=github` set at workflow level
- [ ] Draft + fork guards on every job; concurrency group per PR
- [ ] `pull-requests: write` permission only if a reporter posts comments
- [ ] Provider key added as a repository secret
- [ ] Each gate proven to fire (exit 4 + annotation) on a test PR
