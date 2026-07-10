#!/usr/bin/env bash
# Cut a release: bump the workspace version, regenerate CHANGELOG.md,
# commit, tag v<version>, and push. The pushed tag triggers the
# .github/workflows/release.yaml binary build.
#
# Usage: scripts/release.sh <patch|minor|major|current>
#   current — tag the existing version without bumping (first release).
set -euo pipefail

level="${1:?usage: release.sh <patch|minor|major|current>}"

# Preconditions: on main, clean tree, up to date with origin.
branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = "main" ] || { echo "must release from main (on $branch)" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean" >&2; exit 1; }
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main is not in sync with origin" >&2; exit 1; }

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
IFS=. read -r major minor patch <<<"$current"
case "$level" in
  major)   new="$((major + 1)).0.0" ;;
  minor)   new="$major.$((minor + 1)).0" ;;
  patch)   new="$major.$minor.$((patch + 1))" ;;
  current) new="$current" ;;
  *) echo "unknown level: $level" >&2; exit 1 ;;
esac

if git rev-parse "v$new" >/dev/null 2>&1; then
  echo "tag v$new already exists" >&2; exit 1
fi

echo "releasing v$new (was $current)"

if [ "$level" != "current" ]; then
  # Bump the single workspace version (crates inherit it).
  sed -i.bak "0,/^version = \"$current\"$/s//version = \"$new\"/" Cargo.toml && rm Cargo.toml.bak
  # Refresh Cargo.lock's version entries.
  cargo update -q --workspace
fi

git-cliff --tag "v$new" -o CHANGELOG.md

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "chore(release): v$new"
git tag -a "v$new" -m "graph v$new"
git push -q origin main "v$new"

echo "v$new pushed — binaries build at: https://github.com/tylerdavis/graph/actions"
