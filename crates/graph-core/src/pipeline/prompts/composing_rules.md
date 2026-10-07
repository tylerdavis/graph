## The plan file

Write the whole plan as one YAML document:

- `version: 2`, then `identifier` (snake_case), `name`, and a one-sentence `description`.
- `input_schema`: a JSON Schema object with a `properties` entry (with `type` and `description`) for every value the plan's user supplies each run, listed in `required`. Steps read them as `{{input.name}}`.
- `steps`: a list, each `{id, tool_name, input}`. Give steps short descriptive ids (`release`, `commits`, `summary`). Omit `reasoning`.
- `output`: a map from result names to templates, such as `notes: "{{summary.text}}"` or `files: "{{files}}"`. It is what the plan returns.

## How to compose

1. Every value the user supplies goes in `input_schema`. Never use `ask` for one.
2. Use one step per capability, with the tool search found for it. A tool that returns the data is always better than inference.
3. Reference only the output fields a tool's output schema or observed shape names. When a tool has neither, reference its whole result: `{{step_id}}`.
4. Pass the goal's specifics as tool arguments when the tool takes them: an identifier format such as LN-123 (as a pattern), a path, a project, a state the goal names exactly.
   - An exact value is known only when the goal quotes or names it ("the Needs verification state"), the tool's schema lists it in an `enum`, or an observed output example shows it. Words in the goal such as open, active, done or closed are not values, and status names such as Completed or Canceled are guesses.
   - When you don't know the exact value, don't filter by one, either in a tool argument or in a `where`. If only some items are wanted, use a `filter` with `infer`: a yes/no question asked per item (`infer: "Is this issue still open, not done, canceled or a duplicate? {{item}}"`, with `concurrency: 8`). If the last step summarizes, give it everything and let it sort the items.
5. `map` runs one tool call per item of a list. `filter` with `where` keeps the items whose field matches. `exit` stops early with a result. `route` picks between different tool calls when the data decides. Use each only when the goal needs it; `map` over an empty list is fine and needs no guard.
   - A `where`, `when` or `if` condition is exactly `{value, op, to}`, and `op` is one of `eq`, `ne`, `gt`, `lt`, `gte`, `lte`, `empty`, `not_empty`, `contains`. There is no negated `contains`: keep the matches with `contains`, or compare a field with `ne`.
   - A `map`'s results are only what its body returns, in input order. When a later step needs each item's own fields next to what the call returned (which issue these comments belong to), make the body a list: the tool call, then a `builtin__reshape` that combines `{{item.…}}` with the call's result. Then `filter` those rows.
   - Count with steps, never with inference: `filter` the list, then use `{{filter_id.count}}`. The same goes for picking items by a field.
   - Outside a `route`, only `{{route_id.result}}` (the last result of the branch that ran) and `{{route_id.branch}}` exist; the steps inside a branch can't be referenced by id. The branches return different shapes, so reference `{{route_id.result}}` whole, never a field of it.
6. `builtin__infer` is for judgment only: summarizing, grouping by meaning, classifying, or writing prose. Use it at most once, as late as possible, with every piece of data it needs interpolated into one self-contained instruction. Never follow it with a review, critique or rewrite pass. Without an `output_schema` it returns `{text}`: reference `{{step_id.text}}`.
   - Don't use inference to second-guess what a tool returned, such as a `filter` with `infer` that drops search matches. A `filter` with `infer` is for selecting by meaning when no exact value is known (rule 4).
   - Give it the tools' full results, such as `{{issues}}`, not a filtered subset, unless the goal excludes items: a progress report needs the finished items as well as the open ones.
7. `builtin__reshape` only copies and renames fields. It cannot compute, group, count, sort, deduplicate, parse or combine lists. Never use it to pass data along: later steps can reference any earlier step directly.
8. Do not use `agent` when the tools listed can do the work in fixed steps. If an open-ended subtask truly needs it, give it an explicit `tools` list.
9. Finish with `output`. No solver, no file writes unless the goal asks for one.
10. Use the fewest steps that do the job: no validation, logging, normalization, or summary-of-a-summary steps. Every step's result must be used by a later step or by `output`; a step nothing reads is a mistake.

## Pairing each item with its result

```yaml
  - id: rows
    tool_name: map
    input:
      over: "{{orders.items}}"
      concurrency: 8
      do:
        - id: shipment
          tool_name: shop__get_shipment
          input: { order_id: "{{item.id}}" }
        - id: row
          tool_name: builtin__reshape
          input:
            shape: { id: "{{item.id}}", customer: "{{item.customer}}", status: "{{shipment.status}}" }
  - id: late
    tool_name: filter
    input:
      over: "{{rows.results}}"
      where: { value: "{{item.status}}", op: eq, to: delayed }
```

## Example

Goal: summarize the source changes in a pull request, file by file, skipping deleted files.

```yaml
version: 2
identifier: pr_source_changes
name: Pull request source changes
description: Summarizes a pull request's changed source files, file by file.
input_schema:
  type: object
  required: [pr]
  properties:
    pr: { type: integer, description: Pull request number }
steps:
  - id: meta
    tool_name: builtin__gh_pr_meta
    input: { pr: "{{input.pr}}" }
  - id: changed
    tool_name: builtin__git_changed_files
    input: { base: "{{meta.base_sha}}", head: "{{meta.head_sha}}", prefix: src/ }
  - id: kept
    tool_name: filter
    input:
      over: "{{changed.changes}}"
      where: { value: "{{item.status}}", op: ne, to: deleted }
  - id: files
    tool_name: map
    input:
      over: "{{kept.items}}"
      concurrency: 8
      do:
        tool_name: builtin__git_file
        input: { path: "{{item.path}}", ref: "{{meta.head_sha}}", max_bytes: 20000 }
  - id: summary
    tool_name: builtin__infer
    input:
      instruction: |-
        Summarize what changed in each file of pull request {{input.pr}} ({{meta.title}}), one short paragraph per file.

        Changed files:
        {{kept.items}}

        Their contents:
        {{files.results}}
output:
  summary: "{{summary.text}}"
```
