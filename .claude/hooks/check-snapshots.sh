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

src_drift="$(git diff --name-only HEAD -- src/cli src/main.rs 2>/dev/null || true)"
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
