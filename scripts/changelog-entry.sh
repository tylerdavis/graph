#!/usr/bin/env bash
# Generate the inferred docs-changelog snippet for one release:
#   changelog.d/<version>/summary.md            (always)
#   changelog.d/<version>/migration.md          (only when migration is needed)
# via the repo-carried `changelog_entry` plan (graph dogfooding itself).
#
# Snippets are generated ONCE per release and committed; they are never
# regenerated. To curate one, edit the files and re-run
# scripts/docs-changelog.sh to recompose docs/changelog.mdx.
#
# Usage: changelog-entry.sh vX.Y.Z   (the version's section must exist in
# CHANGELOG.md — at release time release.sh regenerates that first).
# GRAPH_BIN overrides the graph binary (defaults to `graph` on PATH).
set -euo pipefail

version="${1:?usage: changelog-entry.sh vX.Y.Z}"
graph_bin="${GRAPH_BIN:-graph}"
command -v "$graph_bin" >/dev/null || { echo "graph not found on PATH (set GRAPH_BIN)" >&2; exit 1; }

if [ -d "changelog.d/$version" ]; then
  echo "changelog.d/$version already exists — curated snippets are never regenerated" >&2
  exit 0
fi

# The version's section of CHANGELOG.md: from its "## vX.Y.Z " heading to
# the next "## ". The trailing space in the needle keeps v0.1.0 from
# matching v0.10.0's heading.
commits=$(awk -v needle="## $version " 'index($0, needle) == 1 {found=1; next} found && /^## / {exit} found' CHANGELOG.md)
[ -n "$commits" ] || { echo "no CHANGELOG.md section for $version" >&2; exit 1; }

out=$(GRAPH_STORAGE=memory "$graph_bin" plan run changelog_entry \
  --input version="$version" --input commits="$commits")

mkdir -p "changelog.d/$version"
jq -er '.summary' <<<"$out" > "changelog.d/$version/summary.md"
if [ "$(jq -r '.migration_needed' <<<"$out")" = "true" ]; then
  jq -er '.migration_prompt' <<<"$out" > "changelog.d/$version/migration.md"
  echo "$version: summary + migration prompt written to changelog.d/$version/ — review before pushing" >&2
else
  echo "$version: summary written to changelog.d/$version/ — review before pushing" >&2
fi
