#!/usr/bin/env bash
#
# Render the PR coverage comment: a table of files whose line coverage changed
# relative to the baseline (latest `main` run), with before/after values and the
# direction of change. The full per-file summary is published as a build
# artifact instead of being inlined here.
#
# Usage: coverage-comment.sh <baseline.json> <current.json>
#
# Both inputs are the normalised shape produced in CI:
#   { "total": <pct>, "files": { "<repo-relative path>": <pct>, ... } }
#
# The baseline argument may point at a missing file (e.g. first run on main, or
# the baseline artifact was unavailable); the comment degrades gracefully.

set -euo pipefail

BASE="${1:-}"
HEAD="${2:?usage: coverage-comment.sh <baseline.json> <current.json>}"

# Minimum change (in percentage points) for a file to be listed, to suppress
# floating-point noise from re-runs that touch nothing.
EPS=0.05

if [[ -n "$BASE" && -f "$BASE" ]]; then
  jq -rn --slurpfile b "$BASE" --slurpfile h "$HEAD" --argjson eps "$EPS" '
    ($b[0]) as $base | ($h[0]) as $head |
    def rnd(x): (x * 100 | round) / 100;
    def pct(x): if x == null then "—" else "\(rnd(x))%" end;
    def arrow(d): if d > 0 then "🔺" elif d < 0 then "🔻" else "▪️" end;

    # Per-file rows: new files, or files whose coverage moved by at least EPS.
    ( [ $head.files | to_entries[]
        | .key as $f | .value as $after
        | ($base.files[$f]) as $before
        | { file: $f, before: $before, after: $after,
            delta: (if $before == null then null else ($after - $before) end) }
        | select(.before == null or ((.delta | fabs) >= $eps))
      ]
      # Largest decreases first (most concerning), new files (null) sort to top.
      | sort_by(.delta // -1e9)
    ) as $rows |

    ( if $base.total == null then null else ($head.total - $base.total) end ) as $totDelta |

    "## Coverage",
    "",
    ( if $totDelta == null
      then "Total: **\(pct($head.total))**"
      else "Total: **\(pct($head.total))** \(arrow($totDelta)) \(rnd($totDelta)) pp vs `main`"
      end ),
    "",
    ( if ($rows | length) == 0
      then "_No per-file coverage changes vs `main`._"
      else
        ( "| File | Before | After | Δ |",
          "|------|-------:|------:|---|",
          ( $rows[]
            | "| `\(.file)` | \(pct(.before)) | \(pct(.after)) | "
              + ( if .delta == null then "🆕 new" else "\(arrow(.delta)) \(rnd(.delta)) pp" end )
              + " |" )
        )
      end ),
    "",
    "<sub>Full per-file summary is attached as the **coverage-summary** build artifact.</sub>"
  '
else
  jq -rn --slurpfile h "$HEAD" '
    ($h[0]) as $head |
    def rnd(x): (x * 100 | round) / 100;
    "## Coverage",
    "",
    "Total: **\(rnd($head.total))%**",
    "",
    "_No baseline available yet (first run, or the `main` baseline artifact was missing). Per-file deltas will appear on PRs once a baseline has been published from `main`._",
    "",
    "<sub>Full per-file summary is attached as the **coverage-summary** build artifact.</sub>"
  '
fi
