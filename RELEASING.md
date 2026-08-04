# Releasing

graph uses semantic versioning, driven by conventional commits.

## Cut a release

```bash
mise run release:patch    # or release:minor / release:major
```

This bumps the single workspace version in `Cargo.toml` and the docs'
`release_version` variable in `docs/docs.json` (the installation page,
download cards, and cookbook image pins render from it), regenerates
`CHANGELOG.md`, and rebuilds the docs changelog page — graph dogfooding
itself: the `changelog_entry` plan infers the release's summary (and a
migration prompt when consumers must act) into
`docs/snippets/changelog/<version>/`, and the `compose_changelog` plan
renders `docs/changelog.mdx`, which imports those snippets (Mintlify
snippets are never published as standalone pages), so each piece of
prose exists in exactly one file. It then commits as
`chore(release): vX.Y.Z`, tags `vX.Y.Z`, and pushes.

**Review `docs/snippets/changelog/<version>/` before pushing onward**:
the summary and the migration verdict are inferred and meant to be
curated. Edit the snippet files freely — they are never regenerated —
then re-run `graph plan run compose_changelog --input tag=""` (only
needed when a snippet appears or disappears; content edits publish on
their own) and commit. Never edit `docs/changelog.mdx` by hand; it is
composed in full every time.
Requires `graph` ≥ v0.10.0 on PATH (`mise run install`); the release
script validates this before touching anything. The pushed tag triggers
`.github/workflows/release.yaml`, which builds and uploads release binaries
(macOS arm64, Linux x86_64) with checksums to the GitHub release.

Preconditions enforced by the script: on `main`, clean tree, in sync with
origin, tag doesn't already exist.

## Choosing the level

- **patch** — fixes and internal changes (`fix:`, `chore:`, `perf:`)
- **minor** — new user-facing capability (`feat:`), backward compatible
- **major** — breaking changes to the CLI surface, config format, plan/tool
  document formats, or the template dialect

Pre-1.0, minor releases may include breaking changes; call them out in the
release notes.

## Commit convention

Conventional commits (`feat:`, `fix:`, `docs:`, `chore:`, `ci:`, `refactor:`,
`test:`) — the changelog is generated from them, so the subject line should
describe the user-visible effect. `test:`/`chore:`/`ci:` commits are excluded
from the changelog.
