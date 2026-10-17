---
summary: Runs one model call over an instruction and returns text or structured JSON.
output:
  description: Without `output_schema`, the result is `{text}`. With it, the result is an object matching that schema instead, with every field the schema declares present.
  type: object
  properties:
    text:
      type: string
      description: The model's answer, when the call passes no `output_schema`.
---

## Behavior

- Returns `{text}` by default.
- With `output_schema`, returns JSON validated against it, so every declared field is present.
- `model` picks a model role; omit it for the chat role.

## When to use it

- Classify, extract or summarize inside a plan.
- Inside a `map`, interpolate `{{item}}` per call instead of passing the whole list.
