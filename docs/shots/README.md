# Workbench screenshot scenarios

Every SVG in `docs/images/workbench/` is generated from a scenario in
this directory by `mise run shots` — an executed headless workbench
session (real agent loop, pipeline, and debugger; only the LLM and tool
I/O are scripted), never a hand-staged mockup. See
`crates/graph-cli/src/workbench/shots.rs` for the spec format and the
harness.

Docs pages embed the SVGs with a plain Mintlify `<Frame>` + `<img>`
(parameterized snippets don't interpolate props into JSX attributes, so
there is deliberately no shared component):

```mdx
<Frame caption="…">
  <img src="/images/workbench/<name>.svg" alt="…" />
</Frame>
```

`plans/` holds demo plans that exist only for these screenshots (decide /
map / exit examples); they validate against the real catalog like any
plan. This directory is not part of the Mintlify nav — `.md` files here
are repo documentation, not site pages.

Regeneration is byte-identical, so a diff in `docs/images/workbench/`
after `mise run shots` means rendering behavior changed — review it like
any code change.
