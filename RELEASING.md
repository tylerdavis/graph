# Releasing

graph uses semantic versioning, driven by conventional commits.

## Cut a release

```bash
mise run release:patch    # or release:minor / release:major
```

This bumps the single workspace version in `Cargo.toml`, regenerates
`CHANGELOG.md` from conventional commits (git-cliff), commits as
`chore(release): vX.Y.Z`, tags `vX.Y.Z`, and pushes. The pushed tag triggers
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
