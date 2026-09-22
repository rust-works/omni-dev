#!/usr/bin/env bash
# Codex PostToolUse hook: format Rust changes made through apply_patch.
# Non-blocking: always exits 0 so a transient fmt failure does not interrupt edits.
set -u

input="$(cat)"
cwd="$(jq -r '.cwd // empty' <<<"$input")"
[ -n "$cwd" ] || exit 0

root="$(git -C "$cwd" rev-parse --show-toplevel 2>/dev/null)" || exit 0

patch="$(jq -r '.tool_input.command // empty' <<<"$input")"
if ! rg -q '^\*\*\* (Add|Update) File: .*\.rs$|^\+\+\+ [^ ]+\.rs$' <<<"$patch"; then
  exit 0
fi

cd "$root" || exit 0
cargo fmt --all >/dev/null 2>&1 || true
