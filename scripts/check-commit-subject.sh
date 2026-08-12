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
