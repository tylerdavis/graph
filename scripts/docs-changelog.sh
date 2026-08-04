#!/usr/bin/env bash
# Compose docs/changelog.mdx: git-cliff (cliff-docs.toml) emits the
# per-release commit skeleton with summary:/migration: markers, and this
# splices in the committed changelog.d/<version>/ snippets — the inferred
# summary paragraph and, when present, the migration prompt (wrapped in a
# copyable fenced block inside an Accordion).
#
# Deterministic given the snippets, so it is safe to re-run any time —
# that is also the curation loop: edit a snippet, re-run this, commit
# both. Arguments pass through to git-cliff (release.sh passes --tag).
set -euo pipefail

command -v git-cliff >/dev/null || { echo "git-cliff not found on PATH" >&2; exit 1; }

skeleton=$(mktemp)
trap 'rm -f "$skeleton"' EXIT
git-cliff -c cliff-docs.toml "$@" -o "$skeleton"

# Summaries render as MDX, where a bare { or < is a JSX parse error that
# breaks the whole docs build (migration prompts are exempt — they land
# inside a fenced code block). Fail here, with the file named, instead.
for f in changelog.d/*/summary.md; do
  [ -e "$f" ] || continue
  if sed 's/`[^`]*`//g' "$f" | grep -q '[<{]'; then
    echo "$f contains a bare < or { outside backticks — wrap code (flags, fields, {{templates}}) in backticks; MDX parses bare braces as JSX" >&2
    exit 1
  fi
done

{
  while IFS= read -r line; do
    case "$line" in
      '<!-- summary:'*' -->')
        v=${line#'<!-- summary:'}; v=${v%' -->'}
        if [ -f "changelog.d/$v/summary.md" ]; then
          cat "changelog.d/$v/summary.md"
          echo
        fi
        ;;
      '<!-- migration:'*' -->')
        v=${line#'<!-- migration:'}; v=${v%' -->'}
        if [ -f "changelog.d/$v/migration.md" ]; then
          echo
          echo '<Accordion title="Migration required — a prompt for your coding agent" icon="wand-magic-sparkles">'
          echo 'Existing plans may need updating. Copy this prompt into your coding agent in the repository that carries your plans:'
          echo
          echo '````markdown'
          cat "changelog.d/$v/migration.md"
          echo '````'
          echo '</Accordion>'
        fi
        ;;
      *)
        printf '%s\n' "$line"
        ;;
    esac
  done < "$skeleton"
} > docs/changelog.mdx

echo "docs/changelog.mdx composed" >&2
