A user tool is one YAML file. Once saved, it's called as `user__<name>` by agents, plan steps and the CLI.

Top-level keys:
- `version`: the file version; leave it out and graph stamps the current one.
- `name` (required): letters, digits, `_` and `-` only. Short and specific: `weather_now`, `gh_pr_checks`.
- `description` (required): what the tool returns and when to use it. Planners pick tools by this text, so name the system, the input and what comes back ("Current weather for a city from wttr.in: temperature, sky, wind").
- `kind` (required): `exec`, `prompt`, `reshape` or `decision`.
- `input_schema`: a JSON Schema object. Give every property a `type` and a `description`, and list the ones that must be present in `required`. The input is checked against it before every call.
- `output_schema`: a JSON Schema for the result. Only declare it when you know the shape; a test run records the observed shape anyway.
- `read_only`: `true` when the tool only reads (fetches, lists, computes), `false` when it writes, sends, posts, deletes or changes anything. Be honest: plans and agents treat a tool that isn't read-only with more care.

Templates in a tool reference only its input: `{{input.city}}`, `{{input.repo}}`. Nothing else exists inside a tool.

`exec` runs a command:
- `command` (required): the executable, such as `curl`, `gh`, `git`, `jq`, `python3`.
- `args`: a list of arguments, each a string that may contain `{{input.*}}` templates. Each list entry is one argument, so no shell quoting is needed, and there is no shell: pipes and `&&` need `command: bash` with `args: ["-c", "<script>", "--", "{{input.x}}"]`, reading the input as `$1`.
- `env`: extra environment variables; values may use `${VAR}` from the parent environment, which is how tokens reach a tool (`GITHUB_TOKEN: "${GITHUB_TOKEN}"`). Never put a secret in the file itself.
- `cwd`: the working directory.
- `timeout_secs`: default 60.
- `output`: `json` (the default) parses stdout as JSON; `text` wraps stdout as `{"text": …}`. Prefer JSON: ask the command for JSON (`gh … --json`, `curl` against a JSON API, `jq -c`).
A non-zero exit, a timeout, or `json` output that doesn't parse comes back as a tool error with stderr.

`prompt` makes one model call:
- `prompt` (required): the instruction, with `{{input.*}}` templates.
- `system`: a fixed system prompt.
- `model`: a model role, default `chat`.
- `output_schema` (top level) makes the result structured JSON; without it the result is `{"text": …}`.
Use it for judgment over text the caller passes in: classify, extract, summarize.

`reshape` copies and renames fields of its input with `shape`, a template tree over `{{input.*}}`. It computes nothing.

`decision` asks a decision model typed questions with `state` and `questions`; use it only when the user asks for one.

Example:

```yaml
name: weather_now
description: Current weather for a city from wttr.in, with temperature in Celsius, conditions and wind.
kind: exec
command: curl
args: ["-sf", "https://wttr.in/{{input.city}}?format=j1"]
read_only: true
input_schema:
  type: object
  required: [city]
  properties:
    city: {type: string, description: "City name, such as Denver"}
```
