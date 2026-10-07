An agent is one YAML file. Once saved, it can be chatted with (`graph chat <name>`) and called from a plan step as `agent__<name>`.

Keys:
- `version`: the file version; leave it out and graph stamps the current one.
- `name` (required): starts with a lowercase letter; lowercase letters, digits and `_` only.
- `description` (required): what the agent is for, written for the people and agents choosing it.
- `model` (required): a configured model role, usually `chat`.
- `tools` (required): the catalog tools the agent may call, as exact names (`linear__get_issue`) or `*` globs (`linear__*`). `[]` means none. List only what the job needs: an agent can do anything its tools can. Never list plan step names (`map`, `exit`, `route`, …) or generated names (`agent__…`, `transfer_to_…`).
- `subagents`: agents this one can run in a fresh context, each called as `agent__<name>`.
- `handoffs`: agents this one can hand the conversation to, each as `transfer_to_<name>`. An agent can't hand off to itself.
- `input_schema`: typed input when the agent runs from a plan step or as a subagent; an object schema with a `description` on every property. Without it, a call takes `{prompt}`.
- `output_schema`: typed output for those calls; an object schema. Without it, a call returns the agent's final reply as `{result}`.
- `max_iterations`: a cap on model rounds.
- `system_prompt` (required): the agent's instructions.

A conversational agent usually has no schemas. A plan-step agent declares `input_schema` when its caller passes structured data, and `output_schema` when later steps read fields from its answer.

`system_prompt` is a template that may reference only `{{session.date}}`, `{{session.user}}`, `{{rules.control_steps}}` and `{{rules.templating}}`. It can't contain any other `{{…}}`, so describe template syntax in words rather than with examples. Write it in the second person: what the agent is for, how it works, which tools to use for what, and when it's done. Keep it short and specific.

Example:

```yaml
name: linear_triager
description: Reads a Linear issue and suggests its team, priority and labels, with a one-line reason for each.
model: chat
tools: [linear__get_issue, linear__list_teams, linear__list_issue_labels]
input_schema:
  type: object
  required: [issue]
  properties:
    issue: {type: string, description: "The issue identifier, such as LN-123"}
output_schema:
  type: object
  required: [team, priority, labels, reasons]
  properties:
    team: {type: string}
    priority: {type: integer}
    labels: {type: array, items: {type: string}}
    reasons: {type: array, items: {type: string}}
system_prompt: |
  You triage one Linear issue. Read it with linear__get_issue, look up the teams and labels that exist, and suggest the team, a priority from 1 (urgent) to 4 (low), and the labels that fit, each with a one-line reason drawn from the issue itself. Suggest only teams and labels that exist.
```
