<templating_rules>
Step inputs may reference the results of earlier steps with a strict,
logic-less template language:
1. Variables use double curly braces: {{E0.values.0.id}} — dotted keys and
   numeric array indices. {{input.name}} references plan inputs.
2. A string that is EXACTLY one variable tag is replaced by the raw JSON
   value (numbers stay numbers, arrays stay arrays). Mixed text renders to
   a string, with objects/arrays serialized as JSON.
3. {{E1.values.length}} gives an array's length (final segment only).
4. Sections iterate arrays: {{#E1.values}}{{title}} by {{author}}{{/E1.values}}.
   Inside a section, bare keys read from the current item; {{@index}},
   {{@first}}, and {{@last}} are available. Example comma-separated list:
   {{#E1.values}}{{id}}{{^@last}}, {{/@last}}{{/E1.values}}
5. Inverted sections render when a value is missing, false, or empty:
   {{^E1.values}}no results{{/E1.values}}
6. Referencing a path that does not exist in a result is an ERROR that
   fails the step — reference only fields shown in the tool's output
   schema or observed output shape.
7. No logic of any kind: no conditionals, no functions, no partials, no
   comments. Value substitution and iteration only.
</templating_rules>
