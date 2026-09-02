### Early Exits
- Use the `exit` tool to end the plan gracefully instead of proceeding with empty or meaningless data: exit with status "success" and a clear message when there is nothing to do, or "error" to assert a failure condition the user should see. Gate it with `when` (a template condition) or `infer` (an LLM judgment) so the plan continues when the gate does not hold; ungated it always exits.
- When the plan is a check or assertion (a CI gate, a validation, drift detection), make the verdict explicit: end with gated `exit` steps — status "error" asserting the failure condition, status "success" when there is nothing to flag — instead of leaving pass/fail to the final answer's prose.
- An `infer` gate (on `exit` or `decide`) uses the `judge` model role by default; add `model` (a named model or role) alongside `infer` to pin that one verdict to a cheaper or stronger model.

### Asking the user
- Use the `ask` tool ONLY for a value the plan cannot obtain any other way: a choice between options only the user can make, a confirmation before an irreversible action, a missing detail no tool exposes. If a tool can fetch it, call the tool; if a model can judge it, use `infer` or `agent`. Every `ask` costs a human's attention, which is the most expensive resource the plan spends.
- `outputSchema` must be a flat object whose properties are primitives (string, number, integer, boolean) or enums — a person fills it in one field at a time. Describe every property: the description is the label the user sees. Prefer an enum over free text when the options are known.
- Later steps reference the answer as {{Ex.answer.field}}; {{Ex.answered}} is false when nobody could be asked.
- Always set `whenUnanswered`. It is "fail" by default, which stops the run when the plan runs somewhere with no human (CI, a headless client). Use `whenUnanswered: "default"` with a `default` value whenever the plan has a sensible unattended behaviour — that is what keeps one plan runnable both interactively and in automation.

### Branching
- Use the `decide` tool when the correct next call depends on a prior result: it runs `then` when the gate holds, otherwise `else` (or just continues when `else` is omitted). `decide` chooses between actions; `exit` ends the plan.
- Gate it with exactly one of `if` or `infer`. A branch is a single tool call ({"toolName": …, "input": …}) or a list of steps; branch step ids must not reuse top-level step ids.
- Later steps reference only the decide step's id — {{Ex.result}} for the chosen branch's output, {{Ex.branch}} for which side ran. Branch-internal step ids are invisible outside the branch.
- Branches may contain `exit` steps — a fired exit ends the WHOLE plan from inside the branch (e.g. then: post a comment and exit success) — and `agent`, `ask`, or `filter` steps, but never `decide`, `map`, or `reduce`; use a plan__* call inside the branch for nested control flow.

### Selection
- Use the `filter` tool to partition a list before acting on it: `over` must resolve to an array, and the gate — exactly one of `where` (a logical condition) or `infer` (a yes/no question judged per item) — is evaluated once per element with {{item}} and {{index}} available.
- Later steps reference {{Ex.items}} (elements that passed, input order) and {{Ex.count}}, plus {{Ex.dropped}} and {{Ex.dropped_count}} for the other half — selection narrows what runs next, never what is known.
- Filter BEFORE iterating whenever a later call cannot handle every element (e.g. keep only changed files that still exist, then map a file-reading tool over {{Ex.items}}). A list built this way stays aligned with whatever is derived from it downstream.
- `infer` costs one judge call per item (`concurrency` runs them in parallel; `model` pins the verdict model); prefer `where` whenever a field comparison can decide.
- Unlike other control steps, `filter` may appear inside `decide`/`map`/`reduce` bodies. Inside a body its {{item}}/{{index}} shadow the enclosing body's within the gate — reference the outer element in `over` (e.g. over: {{item.children}}), not inside `where`.

### Iteration
- Use the `map` tool to run the same body once per element of a list, and `reduce` to fold a list into a single value. `over` must resolve to an array — usually a whole-list reference like {{E0.issues}}.
- Inside a `map` body, {{item}} is the current element and {{index}} its 0-based position. A `reduce` body also gets {{accumulator}} (the running value, starting at `initial`), and each run's result becomes the next {{accumulator}}.
- Later steps reference only the step's id — {{Ex.results}} for map's per-item outputs (input order) and {{Ex.count}}, or {{Ex.result}} for reduce's final accumulator. Body-internal step ids are invisible outside the body.
- `map` accepts `concurrency` (default 1) to run independent items in parallel. `reduce` is always sequential — for parallel per-item work, map first, then reduce over {{Ex.results}}.
- For inference over a list (classify, summarize, or score each element), prefer `map` with a per-item inference call in the body over interpolating the whole list into one instruction: small, focused contexts are cheaper and more accurate, and `concurrency` recovers the speed. Interpolate a whole list into one inference only when the question is genuinely cross-item (ranking, deduplication, aggregation).
- Bodies may contain `agent`, `ask`, and `filter` steps, but never `exit`, `decide`, `map`, or `reduce`; use a plan__* call inside the body for nested control flow. An `ask` inside a `map` body asks once per item, in item order even when `concurrency` is above 1 — prefer asking once about the whole list.
