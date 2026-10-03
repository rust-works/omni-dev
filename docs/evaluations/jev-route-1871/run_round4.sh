#!/bin/bash
# Replay the round-4 live runs and rebuild the summary. Needs configured Jev
# credentials and writes new files only (the harness refuses to overwrite).
#
# Usage: run_round4.sh WORKTREE OUT_DIR
#   WORKTREE  absolute path to an omni-dev checkout containing this directory
#   OUT_DIR   an empty directory for the six run archives and the summary
set -euo pipefail
WT=${1:?worktree path}
OUT=${2:?empty output directory}
IN="$WT/docs/evaluations/jev-route-1871"
cargo build --manifest-path "$WT/Cargo.toml" --example jev_route_signal_eval
BIN="$WT/target/debug/examples/jev_route_signal_eval"

# A: the nine census cases, as frozen; B: the same without the triage class
# paragraph plus the two prerequisite simulations; C: the spike candidates.
"$BIN" "$IN/round4-inputs.json" "$OUT/A.json" 3 --seed 1
"$BIN" "$IN/round4-variant-inputs.json" "$OUT/B.json" 3 --seed 1
"$BIN" "$IN/round4-spike-inputs.json" "$OUT/C.json" 2 --seed 1
# D1/D2: the production request shape (all three ladders, effort advice) on a
# subset: four implementation cases and six spike cases (three positives, a
# conditional plan and two further negatives).
"$BIN" "$IN/round4-inputs.json" "$OUT/D1.json" 2 --seed 1 \
  --ladders anthropic,openai,gemini --effort-advice \
  --only succinctly-2800-r4,succinctly-2799-r4,succinctly-2709-r4,succinctly-2598-r4
"$BIN" "$IN/round4-spike-inputs.json" "$OUT/D2.json" 2 --seed 1 \
  --ladders anthropic,openai,gemini --effort-advice \
  --only succinctly-2663-r4,succinctly-3685-r4,succinctly-1993-r4,succinctly-2607-r4,succinctly-2784-r4,succinctly-2708-r4
# E: the exploratory absorbed-work simulations written after the first scores.
"$BIN" "$IN/round4-posthoc-inputs.json" "$OUT/E.json" 3 --seed 1

python3 "$IN/summarize_round4.py" "$OUT/summary.json" \
  A="$OUT/A.json" B="$OUT/B.json" C="$OUT/C.json" D1="$OUT/D1.json" D2="$OUT/D2.json" E="$OUT/E.json"
