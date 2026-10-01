# Tasks: decision models, sub-PR 2 (`route` and named cases)

Design: the artifact's sections 3 and 4, and `please-research-jev-and-memoized-oasis.md`. This continues plan v2 with no further bump.

Decisions carried in:
- **`else` keeps one meaning:** "the decision did not commit to a named branch". There is no separate `fallback` key.
- **Named cases are `decide`-only.** `infer` gets no `options`, because an LLM has no calibrated confidence to put behind `min_confidence`.

## [x] Task 1: Rename the `decide` step to `route`
- **Change:**
  - `DECIDE_TOOL` becomes `ROUTE_TOOL = "route"`, and `decision.rs` becomes `route.rs`.
  - The tool def, messages, and bus/trace names follow.
  - The planner prompts and the workbench strings follow.
  - The v1 → v2 migration rewrites `tool_name: decide` (and `toolName`) to `route` in every step list, with a note.
  - The v2 fixtures use `route`.
- **Acceptance:**
  - A v1 plan using `decide` loads, migrates, and runs as `route`.
  - `decide` in a v2 file is an unknown tool.
  - The reserved-name lists say `route`.
- **Verify:** `mise run test`, and `plan validate` on the repo's `.graph/plans`.

## [x] Task 2: Named cases
- **Change:**
  - `decide.options` (`{key: description}`, 2–255 identifier keys) turns the gate into a choice.
  - `route` takes `cases: {key: body}`, and its keys must equal the `options` keys. `else` runs below `min_confidence` on `confidence`.
  - The result is `{branch, choice, confidence, probabilities, result}`.
  - `options` on `exit` or `filter` is still rejected.
- **Acceptance:**
  - Each case runs only its own body, with bus path `E2/<key>.0`.
  - Low confidence takes `else`. Without an `else`, the step continues with `branch: null`.
  - Validation reports mismatched keys, mixing `then`/`else` with `cases`, an unreachable `else`, and fewer than 2 or more than 255 options.
- **Verify:** pipeline tests with a mock choice decider.

## [x] Task 3: Workbench N-way junction
- **Change:** a route with `cases` renders one arm per case plus `else`, wherever `then`/`else` render today. That covers the plan tab, step form, and bus paths.
- **Verify:** the workbench tests, and a screenshot for a `cases` route.

## Task 4: Docs and planner surface
- **Change:**
  - Rename `branching.mdx` content to `route`, with a migration note, and add the named-cases section.
  - Update `plan-schema.mdx` and the gate docs, and replace `decide` step references across `docs/`.
  - Update the planner rules and the tool def.
- **Verify:** `docs_drift` and `format_drift` locally. No `decide` step reference remains.

## Task 5: Review and open the sub-PR
- Run lint, the tests, and `graph_review_core`.
- Push `dm/route` and open the PR into `dev/decision-models`.
