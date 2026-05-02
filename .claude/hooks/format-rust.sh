#!/usr/bin/env bash
# PostToolUse hook: run rustfmt on the edited Rust source file.
# Non-blocking: always exits 0 so a transient fmt failure does not interrupt edits.

set -u

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

file_path="$(jq -r '.tool_input.file_path // empty')"

if [ -n "$file_path" ] && [ -f "$file_path" ]; then
  rustfmt "$file_path" >/dev/null 2>&1 || true
fi

exit 0
