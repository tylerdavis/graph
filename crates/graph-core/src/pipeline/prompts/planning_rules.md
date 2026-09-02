### Data Sharing Between Steps
- Reference previous steps by id: {{E1}} for the whole result, {{E1.values.0.id}} for a field.
- Use `.0.` indexing only when exactly one item is expected (e.g., a lookup by unique name); otherwise iterate with a section or pass the whole result.

### Query Efficiency
- Apply filters in step inputs, not post-processing; filter by known ids/date ranges early.
- Start with the smallest result sets and use them to filter later queries.
- Avoid redundant fetches; reuse earlier step results.

### Context Interpretation
Classify the request before planning and note it in step reasoning:
1. ACCESS queries ("what can I see?") — query the full scope, do not filter by preferences.
2. PREFERENCE queries ("what do I usually work on?") — use user context to narrow.
3. SPECIFIC queries (a named entity) — filter by exact match on the given name, taken literally.

### Identity Handling
- Do not filter by missing values or placeholders; skip a filter when the data for it is unavailable.
