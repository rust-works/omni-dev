#!/usr/bin/env python3
"""Score the Voxtral streaming sweep: WER + latency curve + prefix-loss (#930).

Reads results/summaries.json + results/transcript_<delay>.txt and the canonical
reference (canonical/scandal_in_bohemia.txt), emits the acceptance-criteria
artifacts as markdown to stdout:

  1. Latency-vs-accuracy curve (one row per transcription_delay_ms).
  2. Prefix-loss comparison (first 30 words of each transcript).

The WER core (tokenize + Levenshtein DP with sub/ins/del counts) is lifted
verbatim from #856's scripts/analyze.py so WER is computed identically across
issues for direct comparability.

Usage:
  spike-voxtral/.venv/bin/python spike-voxtral/parse_results.py \
      --results spike-voxtral/results \
      --reference spike-voxtral/canonical/scandal_in_bohemia.txt
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path


# --- WER core, lifted from issue-856 scripts/analyze.py ---------------------
def tokenize(s: str):
    out, w = [], []
    for c in s.lower():
        if c.isalnum() or c == "'":
            w.append(c)
        else:
            if w:
                out.append("".join(w))
                w = []
    if w:
        out.append("".join(w))
    return out


def wer(ref: str, hyp: str):
    """Return (wer, substitutions, insertions, deletions, n_ref, n_hyp)."""
    r = tokenize(ref)
    h = tokenize(hyp)
    n, m = len(r), len(h)
    if n == 0:
        return (0.0 if m == 0 else 1.0, 0, m, 0, n, m)
    dp = [[0] * (m + 1) for _ in range(n + 1)]
    op = [[0] * (m + 1) for _ in range(n + 1)]
    for i in range(n + 1):
        dp[i][0] = i
        op[i][0] = 3  # deletion
    for j in range(m + 1):
        dp[0][j] = j
        op[0][j] = 2  # insertion
    op[0][0] = 0
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            if r[i - 1] == h[j - 1]:
                dp[i][j] = dp[i - 1][j - 1]
                op[i][j] = 0
            else:
                sub = dp[i - 1][j - 1] + 1
                ins = dp[i][j - 1] + 1
                dele = dp[i - 1][j] + 1
                mc = min(sub, ins, dele)
                dp[i][j] = mc
                op[i][j] = 1 if mc == sub else (2 if mc == ins else 3)
    i, j = n, m
    s = ins = dele = 0
    while i > 0 or j > 0:
        o = op[i][j]
        if o == 0:
            i, j = i - 1, j - 1
        elif o == 1:
            s += 1; i, j = i - 1, j - 1
        elif o == 2:
            ins += 1; j -= 1
        else:
            dele += 1; i -= 1
    return ((s + ins + dele) / n, s, ins, dele, n, m)
# ---------------------------------------------------------------------------


# Parakeet streaming baseline for the prefix-loss comparison (from #930 / #898).
PARAKEET_NOTE = (
    "Parakeet (candle streaming, #898/#901): first ~13 s of audio produce NO "
    "output — the first 2 add_audio calls emit zero tokens, dropping ~27 prefix "
    "words on this fixture."
)


def first_words(text: str, k: int = 30) -> str:
    return " ".join(text.split()[:k])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    here = Path(__file__).parent
    ap.add_argument("--results", type=Path, default=here / "results")
    ap.add_argument("--reference", type=Path, default=here / "canonical/scandal_in_bohemia.txt")
    args = ap.parse_args()

    summaries = json.loads((args.results / "summaries.json").read_text())
    ref = None
    if args.reference.exists():
        # Drop the provenance header (#-prefixed lines) so it doesn't pollute WER.
        ref = "\n".join(
            ln for ln in args.reference.read_text(encoding="utf-8").splitlines()
            if not ln.lstrip().startswith("#")
        ).strip()
    if ref is None:
        print(f"WARNING: reference {args.reference} missing — WER columns will be N/A\n")

    print("## Latency vs accuracy curve\n")
    print("Fixture: `tests/fixtures/voice/monologue_5min.wav` (5 min). "
          "WER vs canonical Gutenberg *A Scandal in Bohemia* Part I (trimmed to "
          "the fixture's spoken span). Real-time-paced 80 ms feeds; see harness "
          "honesty note.\n")
    hdr = ("| transcription_delay_ms | effective delay (ms) | first-Partial (ms) | "
           "end-of-utterance (ms) | RTF | max feed lag (ms) | streaming WER |")
    print(hdr)
    print("|---|---|---|---|---|---|---|")
    rows = []
    for s in sorted(summaries, key=lambda x: x["delay_ms"]):
        d = s["delay_ms"]
        tpath = args.results / f"transcript_{d}.txt"
        hyp = tpath.read_text(encoding="utf-8").strip() if tpath.exists() else ""
        if ref:
            w, sub, ins, dele, nref, nhyp = wer(ref, hyp)
            wer_str = f"{w*100:.2f} %"
        else:
            wer_str = "N/A"
        print(f"| {d} | {s['effective_delay_ms']} | {s['first_partial_wall_ms']} | "
              f"{s['end_of_utterance_ms']} | {s['rtf']} | {s['max_feed_lag_ms']} | {wer_str} |")
        rows.append((d, hyp, wer_str))

    print("\n## Prefix-loss comparison (first 30 words)\n")
    print(f"**Parakeet baseline:** {PARAKEET_NOTE}\n")
    if ref:
        print(f"**Canonical reference (first 30):** {first_words(ref)}\n")
    for d, hyp, _ in rows:
        print(f"**Voxtral @ {d} ms (first 30):** {first_words(hyp)}\n")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
