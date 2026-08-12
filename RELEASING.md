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

### What is the source of truth

**The tag is.** Which commits are in a release, its date, their subjects
and grouping — all of that lives in git and is derived on demand through
`git-cliff`. Nothing about it is stored a second time.

The one thing git cannot supply is the prose: the inferred, then curated,
summary and migration prompt. That is the only content this repo stores
for a release, in `docs/snippets/changelog/<version>/`.

So there is one set of facts and three renderings of it:

| Rendering | Built from | Regenerated |
|---|---|---|
| `CHANGELOG.md` | tags, via git-cliff | in full, every release |
| `docs/changelog.mdx` | tags × the prose snippets | in full, every release |
| GitHub release notes | PR titles, via `--generate-notes` | per release |

`CHANGELOG.md` is an **output**, never an input — no plan reads it. It
used to be scraped for the inference evidence, which coupled the docs
build to its heading format; `release_subjects` now reads the tag
directly instead.

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

The changelog's audience is graph's users, not graph's developers. Work on
this repo's own dogfooding — the review plans under `.graph/`, the drift
gate, the CI workflows, the release plans — ships to nobody, so it is typed
`ci:` and stays out of the release notes. Ask before typing a commit: would
someone who installs this version get this change? If not, it is `ci:`.

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

`cliff.toml` has two backstops, and neither replaces getting the type right.
`exclude_paths` drops a commit only when *every* file it touches is excluded,
and the docs-parity invariant makes plan edits touch
`docs/cookbook/ci-checks.mdx` as well. The skip list above the group parsers
is a fixed set of historical corrections for commits that predate this rule —
don't extend it.

## Corrections are retroactive, by design

Because every rendering derives from the tags, a `cliff.toml` change rewrites
the published history of *every* release, not just the next one. That is what
you want when the config was wrong — the fix reaches the releases that were
already wrong — but it has two edges worth knowing:

- **Hand edits to `CHANGELOG.md` do not survive.** It is rewritten in full at
  every release. A correction to a released section has to go in `cliff.toml`;
  that is why the historical skip list exists.
- **Read the whole diff after any `cliff.toml` change.** Diff `CHANGELOG.md`
  and `docs/changelog.mdx` and check what moved in the *older* sections, not
  just the new one.

To correct the prose of a past release, edit its snippet and re-run
`compose_changelog` — snippets are never regenerated, so that sticks.
