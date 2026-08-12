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
`test:`, plus `perf:`, `style:`, `build:`, `revert:`) — the changelog is
generated from them, so the subject line should describe the user-visible
effect. `test:`/`chore:`/`ci:`/`style:` commits are excluded from the
changelog.

The convention is enforced, not just documented, because git-cliff reports
every non-conforming subject as a parse error and the count only ever grows:

- **`scripts/check-commit-subject.sh`** is the single validator. The
  commit-msg hook, CI, and anyone running it by hand all call it.
- **`mise run hooks`** points `core.hooksPath` at `.githooks/`, so the
  `commit-msg` hook rejects a bad subject before the commit exists — for every
  author in the clone, human or agent, and across every worktree (git keeps
  `core.hooksPath` in the shared config). Run it once per clone.
  `git commit --no-verify` bypasses it; CI does not.
- **`.github/workflows/commit-lint.yaml`** checks the PR title and every
  non-merge commit in the PR. It has no `paths-ignore`: a docs-only PR still
  lands commits on `main`.
- **The repo allows squash merges only**, with the squash subject taken from
  the PR title. Merge-button merge commits (`Merge pull request #N from …`)
  were the single largest source of parse errors — 30 of 41 — and squash-only
  means they cannot be created. It also makes the linted PR title the exact
  subject that reaches `main`.

Two classes of commit reach `main` without an author here: merge commits and
the Mintlify app's `Updated mintlify pages` write-backs. `cliff.toml`'s
`commit_preprocessors` rewrite those into conventional form so the existing
skip parsers drop them silently, instead of counting them as parse errors.
With that in place `git-cliff` runs warning-free, so any future warning is a
real, actionable one.
