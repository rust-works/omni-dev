#!/usr/bin/env bash
# Stop hook: block stop when CLI surface changed without snapshot updates.
#
# CLAUDE.md mandates running the update-snapshots skill before declaring
# work done if `src/cli/**` or `src/main.rs` changed. This hook enforces
# that gate by exiting non-zero with a stderr message, which Claude Code
# surfaces back to the model.

set -u

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

input="$(cat)"

# Avoid infinite loops: if Claude Code has already fired this Stop hook
# and is asking us again, defer.
if [ "$(jq -r '.stop_hook_active // false' <<<"$input")" = "true" ]; then
  exit 0
fi

# True when every changed hunk in $1 sits at or below the file's first
# `#[cfg(test)]` line — i.e. the edit is confined to the test module and
# therefore cannot alter CLI surface.
#
# Precise rather than heuristic: clap's surface (a `#[derive(Parser)]`, an
# `#[arg(...)]`, a doc comment that becomes help text) is always declared
# above the test module, so skipping test-only files does not weaken the
# real check. A file with no `#[cfg(test)]` is never treated as test-only.
#
# Running `cargo insta test` would be the fully accurate check, but it
# builds and runs a ~30s test binary — far too slow for a Stop hook.
is_test_only() {
  local file="$1" test_line hunk_start

  # Line numbers are read from the working tree, so hunks must be compared
  # on their new side (`+`), which indexes the same file.
  test_line="$(grep -n '^#\[cfg(test)\]' "$file" 2>/dev/null | head -1 | cut -d: -f1)"
  [ -n "$test_line" ] || return 1

  while read -r hunk_start; do
    [ -n "$hunk_start" ] || continue
    [ "$hunk_start" -lt "$test_line" ] && return 1
  done < <(git diff -U0 HEAD -- "$file" 2>/dev/null |
    sed -n 's/^@@ -[0-9,]* +\([0-9]*\).*/\1/p')

  return 0
}

changed="$(git diff --name-only HEAD -- src/cli src/main.rs 2>/dev/null || true)"

src_drift=""
while IFS= read -r file; do
  [ -n "$file" ] || continue
  # A deleted file has no contents to classify; treat it as surface.
  if [ -f "$file" ] && is_test_only "$file"; then
    continue
  fi
  src_drift="${src_drift}${file}"$'\n'
done <<<"$changed"

src_drift="$(printf '%s' "$src_drift" | sed '/^$/d')"
if [ -z "$src_drift" ]; then
  exit 0
fi

snap_drift="$(git diff --name-only HEAD -- tests/snapshots 2>/dev/null || true)"
if [ -n "$snap_drift" ]; then
  exit 0
fi

{
  echo "CLI surface changed but no snapshot updates are staged:"
  echo "$src_drift" | sed 's/^/  - /'
  echo
  echo "Run the update-snapshots skill (or 'cargo insta test --test integration_test')"
  echo "before stopping. See .claude/skills/update-snapshots/SKILL.md."
} >&2

exit 2
