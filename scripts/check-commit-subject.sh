#!/usr/bin/env bash
# Validate a commit subject (or PR title) against the conventional-commit
# convention this repo's changelog is generated from (see cliff.toml,
# RELEASING.md). Single source of truth: the commit-msg hook, CI, and anyone
# running it by hand all call this.
#
#   scripts/check-commit-subject.sh "feat(cli): add a thing"
#
# Exits 0 when the subject conforms, 1 otherwise (with an explanation on stderr).
set -euo pipefail

subject=${1-}

# Types that git-cliff's commit_parsers know about. Anything else is a typo or
# an invented type, and would silently vanish from the changelog.
types='feat|fix|perf|docs|refactor|test|chore|ci|style|build|revert'

# Machinery we do not author and cannot rename: merge commits, git's own
# fixup/squash prefixes, and the Mintlify app's write-back commits. cliff.toml
# rewrites these into skipped commits rather than reporting them as parse errors.
if [[ $subject =~ ^(Merge\ |Revert\ \"|fixup!|squash!|amend!|Updated\ mintlify\ pages) ]]; then
  exit 0
fi

# The scopes config, plan, tool, and store are reserved: they mean "this
# commit bumps that file kind's version" (RELEASING.md > "Version bumps"),
# and the changelog files the commit under that kind's own section. Every
# such bump is breaking for older binaries, so the `!` marker is mandatory —
# a reserved scope without it is either a misfiled crate change (pick another
# scope: graph-config, plans, tools, storage) or a bump missing its marker.
if [[ $subject =~ ^($types)\((config|plan|tool|store)\):\ .+ ]]; then
  scope=${BASH_REMATCH[2]}
  cat >&2 <<EOF
Commit subject uses the reserved scope "$scope" without the breaking marker:

    $subject

The scopes config, plan, tool, and store are reserved for file-version bumps
and require \`!\`, e.g.

    feat($scope)!: <what changed in the file's schema>

    BREAKING CHANGE: $scope version <N> (graph $scope migrate)

For an ordinary change to the crate that reads those files, use a different
scope (graph-config, plans, tools, storage) or none. See RELEASING.md >
"Version bumps".
EOF
  exit 1
fi

# Deliberately no length limit: this repo's subjects describe the user-visible
# effect and several legitimate ones run long.
if [[ $subject =~ ^($types)(\([a-z0-9._/-]+\))?!?:\ .+ ]]; then
  exit 0
fi

cat >&2 <<EOF
Commit subject does not follow the conventional-commit convention:

    $subject

Expected: <type>[(scope)][!]: <description>
Types:    ${types//|/, }

The changelog is generated from these subjects by git-cliff (cliff.toml), so a
non-conforming subject is either dropped from the changelog or reported as a
parse error. Write the subject as the user-visible effect of the change, e.g.

    feat(workbench): show each step's output contract in the detail pane
    fix: explain missing env vars at first use instead of dying silently
    chore(release): v0.12.0

See RELEASING.md > "Commit convention".
EOF
exit 1
