# Releasing

graph uses semantic versioning, driven by conventional commits.

## Cut a release

```bash
mise run release:patch    # or release:minor / release:major — prepare, then stop
# review docs/snippets/changelog/<version>/ (and CHANGELOG.md)
mise run release:publish  # commit, tag, push
```

Two steps, with the review in between. **Prepare** bumps the single
workspace version in `Cargo.toml` and the docs' `release_version` variable
in `docs/docs.json` (the installation page, download cards, and cookbook
image pins render from it), regenerates `CHANGELOG.md`, and rebuilds the
docs changelog page — graph dogfooding itself: the `changelog_entry` plan
infers the release's summary (and a migration prompt when consumers must
act) into `docs/snippets/changelog/<version>/`, and the `compose_changelog`
plan renders `docs/changelog.mdx`, which imports those snippets (Mintlify
snippets are never published as standalone pages), so each piece of prose
exists in exactly one file. Nothing is committed: the tree is left holding
exactly the release's files for you to read and edit.

**Publish** re-runs `compose_changelog` (so a snippet you added or removed
during the review reaches the page; content edits need nothing), commits
as `chore(release): vX.Y.Z`, tags `vX.Y.Z`, and pushes. It refuses a tree
with changes outside the release's file set, and it re-derives everything
the tag needs from the same facts prepare used rather than trusting a
scratch file. `mise run release:abort` drops a prepared release instead.

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

**The review between prepare and publish is for
`docs/snippets/changelog/<version>/`**: the summary and the migration
verdict are inferred and meant to be curated. Edit the snippet files
freely — they are never regenerated, and publish recomposes the page from
them. To correct a release that is already out, edit its snippet, re-run
`graph plan run compose_changelog --input tag=""`, and commit. Never edit
`docs/changelog.mdx` by hand; it is composed in full every time.
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

The script enforces the floor: if any commit since the last tag is breaking
(a `!` subject or a `BREAKING CHANGE:` footer, as git-cliff reports it) or
any file-version constant moved, `patch` is refused; from 1.0 on, `minor` is
refused too.

## Changelog entries

The changelog is a flat list of entries, one per **module version**, newest
first. Cutting a binary release produces a `Graph v0.13.0` entry; a release
that also bumps a file version produces one more entry per bumped kind —
`Config v2`, `Plan v2`, `Tool v2`, `Store v2` — each with its own heading and
the same date (on the docs page, each `<Update>` card is also tagged with
its module name, so readers can filter to one). Nothing is nested: a reader looking for what changed in the
config schema finds a `Config v2` entry, not a subsection of the binary's.

Which commits go where:

- a **breaking commit scoped `config`, `plan`, `tool`, or `store`** is that
  kind's entry, when the release bumps that kind;
- everything else is the `Graph` entry;
- a kind whose version is new in this release (its first constant) gets an
  entry that says `Introduced with graph vX.Y.Z` when no commit is scoped to
  it.

Both renderings — `CHANGELOG.md` via `cliff.toml`, `docs/changelog.mdx` via
the `cliff_context` tool — read the bumps from the tag message's
`file versions:` line (`config 2 (from 1), plan 1, tool 1, store 1 (new)`),
which `release.sh` writes. The tag stays the source of truth: a regeneration
produces the same entries.

The four scopes are therefore reserved: they mean "this commit bumps that
file kind's version", nothing else. `check-commit-subject.sh` rejects a
reserved scope without `!` (an ordinary change to the crate that reads those
files takes another scope: `graph-config`, `plans`, `tools`, `storage`, or
none), and `release.sh` checks the other direction before cutting: a moved
constant needs a `(kind)!` commit whose `BREAKING CHANGE:` footer names the
new version, and a `(kind)!` commit needs a moved constant.

Breaking commits are exactly what git-cliff reports as breaking: a `!` before
the colon, or a `BREAKING CHANGE:` footer. Either sets the commit's
`breaking` flag; the footer text (or, without one, nothing) is what prints
after the subject. `protect_breaking_commits` keeps a skip parser from ever
hiding one.

## Version bumps

`config.toml`, plan documents, tool documents, and the data directory each
carry an integer file version, independent of the binary version
(`docs/reference/file-versions.mdx` is the user-facing contract). **Every** schema
change to one of those files — a new optional key just as much as a
removed or renamed one, a changed shape or meaning, a newly required key —
is a **version bump**: the number is the schema's generation, and a binary
reads a window of generations (`*_FORMAT_OLDEST..=*_FORMAT`) that narrows
only at a major release. A bump ships as one PR containing all of:

1. The constant raised: `CONFIG_FORMAT` (`crates/graph-config/src/format.rs`),
   `PLAN_FORMAT` / `TOOL_FORMAT` (`crates/graph-core/src/format.rs`), or
   `STORE_FORMAT` (`crates/graph-store/src/file.rs`).
2. A migration appended to that file kind's chain — a forward-only function
   from version N to N+1 over the raw document, returning notes for anything
   it could not carry over. An additive change appends a no-op step.
3. A frozen fixture pair: the version N file stays under
   `tests/fixtures/v<N>/`, its version N+1 twin lands under `v<N+1>/` with the
   same file name, and the golden-pair test proves they load identically.
   Fixtures for every version in the window must keep loading.
4. A breaking commit scoped to the file kind (`feat(config)!: …`) whose
   footer names the new version — `BREAKING CHANGE: config version 2 (graph
   config migrate)` — which is what gives it the changelog's own `Config v2`
   entry. That entry is the record of what changed; the file-versions page
   states the contract and never lists versions. The release script refuses
   to cut without the commit.

The `format_drift` check (`.graph/plans/format_drift.yaml`, run by
`graph-checks.yaml`) fails a PR that changes a model file's schema without
step 1; the fixture tests catch a removed or renamed key mechanically. At
release time the script diffs the four constants between the last tag and
`HEAD`, checks step 4 both ways, writes the result into the
tag message (`file versions: config 2 (from 1), plan 1, tool 1, store 1` —
the tag is the source of truth, so the changelog reads it from there on
every regeneration), and passes the delta to `changelog_entry` as its
`formats` input, which forces `migration_needed` and builds the migration
prompt around `graph config check` and the `migrate` commands rather than
inferring from commit subjects. Move any `ghcr.io/tylerdavis/graph:vX.Y.Z`
pin in `.github/workflows/` in the same PR that stamps this repo's own
`.graph/` files — an older image refuses a newer file version by name.

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
