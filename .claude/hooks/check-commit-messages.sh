#!/usr/bin/env bash
# Stop hook: validate commits ahead of the default base before ending a turn.
# Never rewrite history; commit-twiddle handles findings in the next turn.

set -u

note() {
  echo "Commit-message check skipped: $*" >&2
}

cd "${CLAUDE_PROJECT_DIR:-.}" || { note "cannot enter project directory"; exit 0; }
input="$(cat)"
command -v jq >/dev/null 2>&1 || { note "jq is unavailable"; exit 0; }

# Match the snapshot hook's re-entrancy guard to avoid a Stop loop.
if [ "$(jq -r '.stop_hook_active // false' <<<"$input")" = "true" ]; then
  exit 0
fi

head="$(git rev-parse --verify HEAD 2>/dev/null)" || exit 0
root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
git_dir="$(git rev-parse --absolute-git-dir 2>/dev/null)" || exit 0

# Mirror utils::ai_scratch: AI_SCRATCH may be direct or git-root:relative;
# otherwise TMPDIR (or /tmp) is used. Namespace by checkout, not shared repo.
scratch="${AI_SCRATCH-${TMPDIR-/tmp}}"
case "$scratch" in
  git-root:*) scratch="$root/${scratch#git-root:}" ;;
esac
namespace="$(printf '%s' "$git_dir" | git hash-object --stdin 2>/dev/null)" || exit 0
cache="$scratch/commit-message-check-$namespace.head"
if [ -f "$cache" ] && [ "$(cat "$cache" 2>/dev/null)" = "$head" ]; then
  exit 0
fi

command -v omni-dev >/dev/null 2>&1 || { note "omni-dev is unavailable"; exit 0; }
mkdir -p "$scratch" 2>/dev/null || { note "scratch directory is unavailable"; exit 0; }
output="$(mktemp "$scratch/commit-message-check.XXXXXXXX")" || {
  note "cannot create temporary report"; exit 0;
}
errors="$(mktemp "$scratch/commit-message-errors.XXXXXXXX")" || {
  rm -f "$output"
  note "cannot create temporary diagnostics"; exit 0;
}
trap 'rm -f "$output" "$errors"' EXIT

omni-dev git commit message check --strict --quiet -o json >"$output" 2>"$errors"
status=$?
case "$status" in
  0)
    # The CLI can return a partial successful report after some API requests
    # fail. Do not cache it (or malformed output) as a fully checked HEAD.
    if [ -s "$errors" ] || ! jq -e '
      (.commits | type == "array") and
      (.summary.error_count == 0) and (.summary.warning_count == 0)
    ' "$output" >/dev/null 2>&1; then
      note "check was incomplete or its report was invalid; retry after recovery"
      [ ! -s "$errors" ] || cat "$errors" >&2
      exit 0
    fi
    ;;
esac
case "$status" in
  0|3)
    # Atomic replacement avoids partially written cache entries. Use the SHA
    # captured before the check so a concurrent commit is checked next time.
    printf '%s\n' "$head" >"$output"
    mv -f "$output" "$cache" 2>/dev/null || note "cannot save checked HEAD"
    exit 0
    ;;
  1|2)
    # Exit 1 also represents credential/API failures. Only a structured report
    # with the matching finding count is evidence of failed validation.
    if jq -e --argjson status "$status" '
      (.commits | type == "array") and
      (if $status == 1 then
         ((.summary.error_count | numbers) > 0) and
         any(.commits[].issues[]; .severity == "error")
       else
         ((.summary.warning_count | numbers) > 0) and
         any(.commits[].issues[]; .severity == "warning")
       end)
    ' "$output" >/dev/null 2>&1; then
      {
        echo "Commit messages need attention before stopping:"
        cat "$output"
        echo
        echo "Run the commit-twiddle skill to review and fix these messages."
        echo "See .claude/skills/commit-twiddle/SKILL.md and STYLE-0023."
      } >&2
      exit 2
    fi
    ;;
esac

note "omni-dev failed without validation findings (exit $status); retry after recovery"
[ ! -s "$errors" ] || cat "$errors" >&2
exit 0
