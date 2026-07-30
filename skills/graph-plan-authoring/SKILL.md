---
name: graph-plan-authoring
description: Draft, edit, validate, and run graph plans from the command line.
  Use when creating a new plan, repairing or extending an existing one, adding
  or reordering steps, changing a plan's inputs or finish mode, or driving the
  plan workbench's authoring tools from a script or an agent instead of the TUI.
---

# Graph Plan Authoring

**graph writes plans. You do not.** A planner model, aimed at this machine's real tool catalog, drafts a validated plan from one sentence — then you fix what it got wrong with single-purpose edit commands. Hand-assembling a plan step by step, or writing plan YAML in an editor, is the slow path and usually the wrong one.

## Start here

```bash
graph plan draft "<what the plan should do, as one self-contained instruction>" --json
```

That is the first command to run for any new plan. It costs one round of inference, takes ~30s, and returns a plan that is already statically valid:

```json
{ "identifier": "summarize_the_commits_between_two_git_re", "ok": true,
  "steps": 4, "problems": [], "savedTo": "./.graph/plans/summarize_the_commits_between_two_git_re.yaml" }
```

Then read what it wrote (`graph plan show <id> --json`), apply the fixes below, validate, and run.

### Before you draft, do not

- **Do not read the docs site first.** Every command is self-describing — `graph plan --help`, `graph plan step add --help`. The reference links at the bottom of this page are for when a rejection message isn't self-explanatory, not for warming up.
- **Do not write or edit plan YAML in a text editor.** The edit commands enforce validation on every write; a text editor doesn't, and any edit reserializes the file anyway.
- **Do not enumerate the whole tool catalog and assemble steps yourself.** The planner already sees the catalog and the [shape cache](https://github.com/tylerdavis/graph/blob/main/docs/tools/shape-cache.mdx). Probe individual tools (`graph tools show <name>`) when you're fixing a specific step, not as a survey.
- **Do not ask the user for a plan's steps.** Ask for the *goal*; the goal is what `draft` consumes.

### Draft, or build by hand?

| Situation | Do this |
|---|---|
| Any new plan, described in prose | `graph plan draft "<goal>"` |
| Drafted plan is wrong in a specific place | edit commands (`plan set`, `plan step *`) |
| Drafted plan is wrong structurally | `graph plan draft "<goal>" --from <plan> --feedback "<what's wrong>"` |
| Adding a step to a plan you already understand | `graph plan step add` |
| **No provider credentials** (offline, CI, no API key) | `graph plan new` + edit commands — the only path that costs zero inference |

Building a whole plan with `plan new` + a dozen `step add` calls is for that last row only. If a planner is reachable, drafting gets there faster and grounds the steps in the real catalog.

## The three fixes a fresh draft usually needs

Verified against a real draft — check each one every time.

**1. The identifier and name are the goal string, truncated.** A draft of "Summarize the commits between two git refs…" lands as `summarize_the_commits_between_two_git_re`. Rename it:

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

Then:

```bash
graph plan validate commit_digest --json
graph plan run commit_digest '{"base":"v0.8.0","head":"main"}' --json
```

## Iterating

**Structural rework — redraft with feedback.** Cheaper in tokens and attention than a chain of hand edits:

```bash
graph plan draft "<goal>" --from commit_digest --feedback "drop the second infer step; group commits with builtin__reshape instead" --json
```

**Surgical fixes — one edit command per intent.** Each resolves the plan, applies one edit, writes the file back. No session, no undo (version control is the undo), safe to run concurrently.

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
- **Run exit codes:** `0` ok, `1` failure, `3` needs input (schema printed), `4` an exit gate fired.

## Checklist

- [ ] Started from `graph plan draft`, not a hand-assembled plan (unless there are no credentials)
- [ ] Renamed the goal-derived identifier and deleted the leftover file
- [ ] Every `{{input.*}}` root declared in `input_schema`
- [ ] Solver `data` keys are short and reference only what the answer needs
- [ ] `graph plan validate <plan> --json` → `ok: true` (notes acceptable)
- [ ] `graph plan run <plan> '<input>'` exits 0 against real input
- [ ] Rejections resolved by fixing the named cause, not by editing YAML directly

## Reference — read only when needed

- A rejection message you can't act on → [`reference/cli.mdx#authoring-plans`](https://github.com/tylerdavis/graph/blob/main/docs/reference/cli.mdx)
- An envelope key not covered above → [`reference/scripting-contract.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/scripting-contract.mdx)
- Writing a control step (`exit`, `decide`, `map`, `reduce`, `agent`) by hand → [`plans/exit-gates`](https://github.com/tylerdavis/graph/blob/main/docs/plans/exit-gates.mdx), [`branching`](https://github.com/tylerdavis/graph/blob/main/docs/plans/branching.mdx), [`iteration`](https://github.com/tylerdavis/graph/blob/main/docs/plans/iteration.mdx), [`agent-step`](https://github.com/tylerdavis/graph/blob/main/docs/plans/agent-step.mdx)
- A template that won't render → [`plans/template-language.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/template-language.mdx)
- Choosing a finish mode → [`plans/finish-modes.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/plans/finish-modes.mdx)
- The full field list → [`reference/plan-schema.mdx`](https://github.com/tylerdavis/graph/blob/main/docs/reference/plan-schema.mdx)
