---
name: graph-plan-authoring
description: Draft, edit, validate, and run graph plans from the command line.
  Use when creating a new plan, repairing or extending an existing one, adding
  or reordering steps, changing a plan's inputs or finish mode, or driving the
  plan workbench's authoring tools from a script or an agent instead of the TUI.
---

# Graph Plan Authoring

The [graph repository](https://github.com/tylerdavis/graph) is the source of truth. Read the current docs before authoring — they move ahead of this skill:

- [`docs/plans/authoring.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/authoring.mdx) — the authoring loop, end to end.
- [`docs/reference/cli.mdx#authoring-plans`](https://github.com/tylerdavis/graph/blob/main/docs/reference/cli.mdx) — every command, every attribute.
- [`docs/reference/scripting-contract.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/scripting-contract.mdx) — the `--json` envelopes and exit codes.
- [`docs/reference/plan-schema.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/plan-schema.mdx), [`docs/plans/template-language.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/template-language.mdx), [`docs/plans/finish-modes.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/finish-modes.mdx).

These commands are the same authoring entry points the [plan workbench](https://github.com/tylerdavis/graph/blob/main/docs/workbench/plan-workbench.mdx) TUI drives, exposed as one-shot commands — so an agent can build a plan without a terminal app and without holding a session open.

**Each command is stateless: resolve a plan, apply one edit, write the YAML back.** There is no draft session and no undo — version control is the undo. Every command is idempotent and safe to run concurrently.

**Only `graph plan draft` costs inference.** `plan new` plus the edit commands author a plan end to end with zero LLM calls and no provider credentials. Prefer building steps directly whenever you already know them: it is free, deterministic, and reviewable as a diff.

## The command surface

```
graph plan new <identifier> [--name <n>] [--description <d>] [--output <path>] [--json]
graph plan draft <goal> [--from <name|path>] [--feedback <f>] [--output <path>] [--stdout] [--json]

graph plan set   <name|path> <attribute> <value>... [--json]
graph plan unset <name|path> <attribute> [--json]

graph plan step add    <name|path> <id> <tool> <json|@file|-> [--reasoning <r>] [--before <id> | --after <id>] [--json]
graph plan step update <name|path> <id> <tool|input|reasoning> <value> [--json]
graph plan step rename <name|path> <id> <new-id> [--json]
graph plan step unset  <name|path> <id> <attribute> [--json]
graph plan step rm     <name|path> <id> [--json]

graph plan list [--json]
graph plan show <name> [--json]
graph plan validate <name|path> [--json]
graph plan run <name> '<json>' [--input k=v]... [--json]
```

`<name|path>` is a plan identifier or a YAML file path. `show` and `validate` deliberately open plans the runnable catalog rejects (invalid, or hidden by unconfigured `requires_servers`) — that is exactly the plan you need to inspect.

**Always pass `--json`.** Without it a one-line result goes to stderr and stdout stays empty; with it you get a structured envelope — including on rejection, where the problem list is what you want most.

## Workflow

### 1. Orient

```bash
graph plan list --json                 # what already exists, and where plans live
graph plan show <name> --json          # the current document, if editing
graph plan validate <name> --json      # its verdict before you touch anything
```

Editing an existing plan: read it first. Never guess at step ids or template paths.

### 2. Probe the tools before writing any step

```bash
graph tools list                       # namespaced catalog names
graph tools show <tool>                # input schema
graph tools test <tool> '{"...":"..."}'  # the *actual* output shape
```

`graph tools *` has **no** `--json` flag — parse the human output, or read the cached shape with `graph shapes show <tool>`. Probing pays twice: you author correct `{{E0.path}}` references, and every call feeds the [shape cache](https://github.com/tylerdavis/graph/blob/main/docs/tools/shape-cache.mdx). Skipping this is the single biggest cause of plans that validate clean and then die mid-run on a path that doesn't exist.

### 3. Create the plan

**Hand-built (free, preferred when the steps are known):**

```bash
graph plan new report --name "Report" --description "roll a team's issues into a digest" --json
```

`new` scaffolds a plan with no steps, so it is *intentionally invalid* on creation (`plan has no steps`). That is fine — edits are only refused if they make things worse, so a scaffold stays editable.

**Drafted (costs inference):**

```bash
graph plan draft "Summarize this week's Linear issues for the Core team" --json
graph plan draft "<goal>" --from report --feedback "E2 references a field that doesn't exist" --json
graph plan draft "<goal>" --stdout            # print the YAML, write nothing
```

Drafting runs the planner over your tool catalog one validated step at a time. If it exhausts retries on a step, the valid prefix is still saved and reported under `salvaged` / `failedStep` — finish it with `step add` rather than redrafting from scratch.

### 4. Header and inputs

```bash
graph plan set report description "Weekly digest of a team's Linear issues" --json
graph plan set report exemplars "How is Core doing this week?" "Weekly Core digest" --json
graph plan set report requires_servers linear --json
graph plan set report input_schema '{"type":"object","required":["team"],"properties":{"team":{"type":"string","description":"Linear team name"}}}' --json
```

| `plan set <attribute>` | Value |
|---|---|
| `name`, `description`, `identifier` | one string |
| `exemplars`, `requires_servers` | one or more strings |
| `input_schema`, `solver`, `output` | JSON object: inline, `@file.json`, or `-` for stdin |

`exemplars` and `description` are routing signals the agent reads, not commentary — write them for a reader deciding whether to call this plan.

### 5. Steps

```bash
graph plan step add report E1 linear__list_issues '{"team":"{{input.team}}","limit":100}' \
  --reasoning "Pull the team's recent issues" --json
graph plan step add report E2 builtin__reshape '{"shape":{"count":"{{E1.length}}"}}' --after E1 --json
```

- `<tool>` is a namespaced catalog name (`linear__…`, `user__…`, `builtin__…`, `plan__…`) or a bare control step: `exit`, `decide`, `map`, `reduce`, `agent`, plus `plan_and_execute`.
- Step ids are identifiers, unique across the whole plan (control-step body sub-steps included), and may not shadow the reserved template roots `input`, `item`, `index`, `accumulator`, `length`.
- Steps run sequentially; a step may reference plan inputs and any *earlier* step only.
- `--before` / `--after` anchor an insertion; the default appends.

Editing:

```bash
graph plan step update report E1 input '{"team":"{{input.team}}","limit":250}' --json
graph plan step update report E1 tool linear__list_issues --json
graph plan step rename report E1 fetch_issues --json     # rewrites every downstream {{E1.…}}
graph plan step unset  report E1 reasoning --json
graph plan step rm     report E2 --json
```

`step update input` replaces the step's **whole** input object — read the step first, then write the merged object back.

### 6. Finish mode

Exactly one of `solver` (prose report, 1 LLM call), `output` (structured JSON, 0 calls), or neither (silent, side effects only). They are mutually exclusive — setting either clears the other.

```bash
graph plan set report output '{"team":"{{input.team}}","count":"{{E2.count}}"}' --json

graph plan set report solver '{"query_to_answer":"Write a digest of {{input.team}}...","data":{"issues":"{{E1.issues}}"}}' --json

graph plan unset report solver --json                    # -> silent plan
```

For anything larger than a line, use `@file.json` or `-` rather than fighting shell quoting on an embedded template.

### 7. Validate, then run

```bash
graph plan validate report --json
graph plan run report '{"team":"Core"}' --json
```

`validate --json` reports every layer at once and separates fatal from local:

```json
{ "plan":"report", "steps":3, "ok":false,
  "problems":["step E2 references E5, which is not `input` or an earlier step"],
  "notes":["step E1: tool 'linear__list_issues' needs MCP server 'linear', which is not configured under [mcp.linear]"] }
```

`problems` make `ok` false and exit `1`. `notes` never do — a plan naming a server *this* machine lacks is still portable and correct.

Run exit codes: **0** ok, **1** failure, **3** needs input (a required field was missing; the schema is printed), **4** an exit gate fired.

## Reading the envelopes

Success on a mutating command:

```json
{ "ok": true, "id": "E2", "index": 1, "steps": 2,
  "savedTo": "./.graph/plans/report.yaml",
  "preExistingProblems": ["plan has no steps"],
  "note": "edit applied; the plan is still invalid, but only from pre-existing problems …" }
```

Rejection — exit `1`, envelope still on stdout:

```json
{ "error": "edit rejected — it would introduce new validation problems (the draft is unchanged)",
  "problemsIntroduced": ["step E2 references E1, which is not `input` or an earlier step"] }
```

`plan new` reports its scaffold's problems as `problems` (the plan is invalid by design); every *subsequent* edit reports them as `preExistingProblems` with an explanatory `note`. Both mean the same thing: the write happened.

Keys worth branching on: `problemsIntroduced` (fix and retry), `preExistingProblems` (informational — the edit *did* apply), `availableSteps` (unknown step id, with what exists), `renamedFrom` (an `identifier` change wrote a new file), `salvaged` / `failedStep` (draft saved a valid prefix).

## Key concepts

**Edits can only improve things.** An edit is rejected only if it introduces a *new* validation problem, and a rejected edit leaves the file untouched — so `step rm` fails while a later step still references the step, naming the template that would dangle. Pre-existing problems never block an edit; otherwise repairing a broken plan would be impossible.

**Edits are not catalog-aware; `validate` is.** `step add` accepts a tool name that doesn't exist in the catalog — the edit guard runs the static layers only. A bogus or misspelled tool surfaces at `graph plan validate`, so always validate after adding steps; don't treat `"ok": true` on `step add` as proof the tool resolves.

**A rejection is not an error to route around.** `problemsIntroduced` is the exact repair list. Fix the referencing step first, then retry the edit — never rewrite the YAML file by hand to dodge the guard.

**Nothing is clobbered silently.** A write refuses a file holding a different plan. Changing `identifier` writes a *new* file and leaves the original in place (`renamedFrom`); deleting the old one is your call.

**Every edit reserializes the file.** Flow mappings become block mappings, folded scalars (`>`) become literal (`|`), and **comments are dropped**. Commentary belongs in `description` and each step's `reasoning` — real fields the agent also reads. Don't mix hand-editing with these commands expecting formatting to survive.

**Logic belongs in steps, not templates.** The template dialect is strict, typed, and logic-less. Conditionals go in `exit` gates and `decide` steps; computation goes in a tool.

**One command, one intent.** Required operands are positional and attributes are `<attribute> <value>` — there is no batch mode. Sequence the calls, checking each envelope, rather than trying to do two things at once.

## Repair loop for an existing plan

1. `graph plan validate <name> --json` → read `problems`.
2. `graph plan show <name> --json` → locate the offending step / template.
3. Probe the real shape (`graph tools test`, `graph shapes show`) before rewriting a path.
4. Apply **one** edit; if it returns `problemsIntroduced`, fix that cause first.
5. Re-validate. Repeat until `ok: true` with only `notes` left.
6. `graph plan run <name> '<input>' --json` against real input.

## Checklist

- [ ] `--json` on every plan command; `graph tools *` parsed as text
- [ ] Tools probed (`tools show` / `tools test`) before any `{{…}}` path was written
- [ ] Steps reference only `input` and earlier steps; no reserved-root ids
- [ ] Exactly one finish mode set (or deliberately silent)
- [ ] `graph plan validate <name> --json` → `ok: true` (notes are acceptable)
- [ ] `graph plan run <name>` exits 0 against real input
- [ ] Rejections resolved by fixing the named cause, not by hand-editing the YAML
