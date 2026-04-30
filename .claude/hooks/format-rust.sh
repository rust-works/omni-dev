#!/usr/bin/env bash
# PostToolUse hook: run `cargo fmt --all` after Claude edits a Rust source file.
# Non-blocking: always exits 0 so a transient fmt failure does not interrupt edits.

set -u

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

cargo fmt --all >/dev/null 2>&1 || true

exit 0
