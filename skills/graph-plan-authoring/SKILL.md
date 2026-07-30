---
name: graph-plan-authoring
description: Drive graph's own commands to draft, write, edit, validate, and
  test plans. Use when creating a new plan, repairing or extending an existing
  one, adding or reordering steps, changing a plan's inputs or finish mode, or
  driving the plan workbench's authoring tools from a script instead of the TUI.
---

# Graph Plan Authoring

**graph drafts, writes, edits, and tests plans. Your job is to drive those commands and judge the results.** Every stage of the lifecycle is already a command — aimed at this machine's real tool catalog, enforcing validation on every write:

| Stage | The command | Do *not* substitute |
|---|---|---|
| Draft | `graph plan draft "<goal>"` | assembling steps yourself from the catalog |
| Write | `graph plan new`, `graph plan set`, `graph plan step add` | writing YAML in an editor |
| Edit | `graph plan set/unset`, `graph plan step update/rename/rm` | patching the file by hand |
| Check | `graph plan validate --json` | reasoning about whether the plan looks valid |
| Test | `graph tools test`, `graph plan run` | predicting what a tool returns, or writing your own harness |

Each one applies a single intent and reports a machine-readable result. Reimplementing any of them by hand is slower, unvalidated, and usually wrong — and drafting is the stage agents skip most often.

## Start here

```bash
graph plan draft "<what the plan should do, as one self-contained instruction>" --json
```

That is the first command to run for any new plan. It costs one round of inference, takes ~30s, and returns a plan that is already statically valid:

```json
{ "identifier": "summarize_the_commits_between_two_git_re", "ok": true,
  "steps": 4, "problems": [], "savedTo": "./.graph/plans/summarize_the_commits_between_two_git_re.yaml" }
```

Then read what it wrote (`graph plan show <id> --json`), apply the fixes below, validate, and run. `draft` is the only authoring command that costs inference; everything after it is free and deterministic.

### Before you draft, do not

- **Do not read the docs site first.** Every command is self-describing — `graph plan --help`, `graph plan step add --help`. The reference links at the bottom of this page are for when a rejection message isn't self-explanatory, not for warming up.
- **Do not write or edit plan YAML in a text editor.** The edit commands enforce validation on every write; a text editor doesn't, and any edit reserializes the file anyway.
- **Do not enumerate the whole tool catalog and assemble steps yourself.** The planner already sees the catalog and the [shape cache](https://github.com/tylerdavis/graph/blob/main/docs/tools/shape-cache.mdx). Probe individual tools (`graph tools show <name>`) when you're fixing a specific step, not as a survey.
- **Do not ask the user for a plan's steps.** Ask for the *goal*; the goal is what `draft` consumes.
- **Do not judge a plan by reading it.** `graph plan validate --json` is the verdict, and `graph plan run` is the proof. Neither costs you a guess.

### Draft once, then edit

| Situation | Do this |
|---|---|
| Any new plan, described in prose | `graph plan draft "<goal>"` |
| Drafted plan is wrong — in one place or in ten | edit commands (`plan set`, `plan step *`) |
| Adding a step to a plan you already understand | `graph plan step add` |
| **No provider credentials** (offline, CI, no API key) | `graph plan new` + edit commands — the only path that costs zero inference |

Building a whole plan with `plan new` + a dozen `step add` calls is for that last row only. If a planner is reachable, drafting gets there faster and grounds the steps in the real catalog.

**Never redraft to fix a plan.** Drafting replaces every step, so aiming the planner at a draft that is 80% right throws away the 80% to fix the 20% — and the next draft is a fresh roll of the dice, not an improvement on this one. However structural the change, sequential edit commands are the way: each applies one intent, is validated atomically, and is *refused* if it would make the plan invalid. Ten rejected-or-applied edits beat one silent rewrite.

The only reason to draft twice is to start over from a genuinely better goal, having decided the current plan is worth nothing.

### Getting a good identifier

`draft` names the plan after the goal string, truncated. To skip the rename, create the identity first and draft into it:

```bash
graph plan new commit_digest --description "Summarize commits between two refs"
graph plan draft "<goal>" --from commit_digest --json
```

`--from` carries the identifier, name, description, and input schema into the draft. It is *not* a revision flag — the steps are still drafted from scratch — so only use it on a plan whose steps you have not hand-tuned yet.

## The three fixes a fresh draft usually needs

Verified against a real draft — check each one every time.

**1. The identifier and name are the goal string, truncated.** A draft of "Summarize the commits between two git refs…" lands as `summarize_the_commits_between_two_git_re`. Avoid this entirely by creating the identity first and drafting into it with `--from` (above). If you already have the ugly draft, rename it:

```bash
graph plan set <ugly-id> identifier commit_digest --json
graph plan set commit_digest name "Commit Digest" --json
rm ./.graph/plans/<ugly-id>.yaml     # `set identifier` writes a NEW file; the envelope's `renamedFrom` names the leftover
```

**2. `input_schema` is often missing while steps reference `{{input.*}}`.** This *validates clean* — no static layer ties template roots to the schema — and then dies at run time:

```
plan failed at step E0 (builtin__git_log): bad path 'input.base': no key 'base' at input (available: )
```

Grep the drafted YAML for `{{input.` and declare every root you find:

```bash
graph plan set commit_digest input_schema '{"type":"object","required":["base","head"],
  "properties":{"base":{"type":"string","description":"Base git ref"},
                "head":{"type":"string","description":"Head git ref"}}}' --json
```

**3. The solver's `data` keys are whole reasoning sentences, and it splices every step.** Drafts routinely emit `"E0 Fetch the commit history between the two refs. This provides…": "{{E0}}"` for all four steps, including `exit` gates that carry nothing. Rewrite it with the couple of results the answer actually needs:

```bash
graph plan set commit_digest solver '{"query_to_answer":"Summarize the notable changes …","data":{"commits":"{{E0.commits}}","analysis":"{{E2}}"}}' --json
```

## Testing what you built

Don't reason about whether a plan works — graph runs it.

```bash
graph plan validate commit_digest --json                          # static: refs, ids, tools resolve
graph tools test builtin__git_log '{"base":"v0.8.0","head":"main"}'   # one tool, real output
graph plan run commit_digest '{"base":"v0.8.0","head":"main"}' --json  # the whole plan, real data
```

`validate` catches what is knowable without running: dangling `{{refs}}`, duplicate ids, tools that don't resolve. Everything else — a path that isn't in a tool's real output, an empty result, a gate that fires — only shows up in a run, as a typed error naming the available keys.

When a step's template path is the suspect, `graph tools test <tool> '<input>'` calls that one tool and prints exactly what it returns; `graph shapes show <tool>` prints what it has returned before. Use those instead of guessing at a field name — and both feed the [shape cache](https://github.com/tylerdavis/graph/blob/main/docs/tools/shape-cache.mdx), which makes the next draft better.

Read the exit code, not just the output: `0` ok, `1` failure, `3` needs input (the schema is printed — that is fix #2 above), `4` an exit gate fired deliberately.

## Iterating

**Every fix is an edit command — one per intent.** Each resolves the plan, applies one edit, writes the file back. No session, no undo (version control is the undo), safe to run concurrently. Structural rework is just several of them in a row: to drop a step and change how the next one groups its data, that is one `step rm` and one `step update`, each validated on the way in.

```bash
graph plan set    <plan> <name|description|identifier|exemplars|requires_servers|input_schema|solver|output> <value>... --json
graph plan unset  <plan> <attribute> --json
graph plan step add    <plan> <id> <tool> '<json>' [--reasoning <r>] [--before <id>|--after <id>] --json
graph plan step update <plan> <id> <tool|input|reasoning> <value> --json
graph plan step rename <plan> <id> <new-id> --json      # rewrites every downstream {{id.…}}
graph plan step unset  <plan> <id> <attribute> --json
graph plan step rm     <plan> <id> --json
```

- `input_schema`, `solver`, and `output` take a JSON object — inline, `@file.json`, or `-` for stdin. Use `@file` for anything with embedded templates rather than fighting shell quoting.
- `step update input` replaces the step's **whole** input object; read the step first, write the merged object back.
- `solver` and `output` are the two finish modes and are mutually exclusive — setting either clears the other; unsetting both leaves a silent plan.
- **Always pass `--json`.** Without it a one-line result goes to stderr and stdout stays empty.

## Reading the envelopes

Applied: `{"ok": true, "savedTo": "…", "preExistingProblems": [...]}` — `preExistingProblems` are informational, the write happened. (`plan new` spells the same thing `problems`.)

Rejected — exit `1`, envelope still on stdout, **file untouched**:

```json
{ "error": "edit rejected — it would introduce new validation problems (the draft is unchanged)",
  "problemsIntroduced": ["step E2 references E1, which is not `input` or an earlier step"] }
```

`problemsIntroduced` is a repair list, not an obstacle: fix the named cause, then retry the edit. Never route around a rejection by hand-editing the file.

Also worth branching on: `availableSteps` (unknown step id), `renamedFrom` (an `identifier` change left the original file), `salvaged` / `failedStep` (drafting ran out of retries and saved the valid prefix — finish it with `step add` rather than redrafting).

## Guard rails to know

- **Edits can only improve things.** An edit is rejected only if it introduces a *new* problem; pre-existing ones never block an edit (otherwise repairing a broken plan would be impossible). So `step rm` fails while a later step still references the step, naming the template that would dangle.
- **Edits are not catalog-aware; `validate` is.** `step add` accepts a tool name that doesn't exist — only the static layers run on a write. Always `plan validate` after adding steps; `"ok": true` on an edit is not proof the tool resolves.
- **`validate`'s `notes` are not failures.** `problems` make `ok` false and exit `1`; a `note` about an MCP server this machine lacks means the plan is portable and correct, just not runnable here.
- **Every edit reserializes the file** — comments are dropped, flow mappings become block. Commentary belongs in `description` and each step's `reasoning`, which are real fields the agent reads.
- **Run exit codes:** `0` ok, `1` failure, `3` needs input (schema printed), `4` an exit gate fired. A plan is done when a real run exits `0`, not when it looks right.

## Checklist

- [ ] Started from `graph plan draft`, not a hand-assembled plan (unless there are no credentials)
- [ ] Renamed the goal-derived identifier and deleted the leftover file
- [ ] Every `{{input.*}}` root declared in `input_schema`
- [ ] Solver `data` keys are short and reference only what the answer needs
- [ ] `graph plan validate <plan> --json` → `ok: true` (notes acceptable)
- [ ] `graph plan run <plan> '<input>'` exits 0 against real input
- [ ] Rejections resolved by fixing the named cause, not by editing YAML directly
- [ ] Every stage went through a graph command — no hand-written YAML, no hand-run harness

## Reference — read only when needed

- A rejection message you can't act on → [`reference/cli.mdx#authoring-plans`](https://github.com/tylerdavis/graph/blob/main/docs/reference/cli.mdx)
- An envelope key not covered above → [`reference/scripting-contract.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/scripting-contract.mdx)
- Writing a control step (`exit`, `decide`, `map`, `reduce`, `agent`) by hand → [`plans/exit-gates`](https://github.com/tylerdavis/graph/blob/main/docs/plans/exit-gates.mdx), [`branching`](https://github.com/tylerdavis/graph/blob/main/docs/plans/branching.mdx), [`iteration`](https://github.com/tylerdavis/graph/blob/main/docs/plans/iteration.mdx), [`agent-step`](https://github.com/tylerdavis/graph/blob/main/docs/plans/agent-step.mdx)
- A template that won't render → [`plans/template-language.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/template-language.mdx)
- Choosing a finish mode → [`plans/finish-modes.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/finish-modes.mdx)
- The full field list → [`reference/plan-schema.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/plan-schema.mdx)
