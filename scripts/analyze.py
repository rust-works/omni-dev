#!/usr/bin/env python3
"""Analyse a spike run's events.jsonl + ground-truth to produce metrics.

Usage: analyze.py <events.jsonl> <expected.txt>

Outputs:
  partial_latency_p50_ms / partial_latency_p95_ms   (proxy: wall_ms - audio_ms at Partial emission)
  time_to_final_mean_ms / time_to_final_max_ms      (wall gap silence_onset → next final)
  rtf                                               (parsed from process stderr if present, else N/A)
  peak_rss_mb                                       (parsed from time -l if piped)
  wer / substitutions / insertions / deletions
  total_final_chars
"""
import json
import re
import statistics
import subprocess
import sys
from pathlib import Path


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
    r = tokenize(ref)
    h = tokenize(hyp)
    n, m = len(r), len(h)
    if n == 0:
        return (0.0 if m == 0 else 1.0, 0, m, 0, n, m)
    dp = [[0] * (m + 1) for _ in range(n + 1)]
    op = [[0] * (m + 1) for _ in range(n + 1)]
    for i in range(n + 1):
        dp[i][0] = i
        op[i][0] = 3
    for j in range(m + 1):
        dp[0][j] = j
        op[0][j] = 2
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
    subs = ins = dele = 0
    while i > 0 or j > 0:
        o = op[i][j]
        if o == 0:
            i -= 1
            j -= 1
        elif o == 1:
            subs += 1
            i -= 1
            j -= 1
        elif o == 2:
            ins += 1
            j -= 1
        else:
            dele += 1
            i -= 1
    edits = subs + ins + dele
    return (edits / n, subs, ins, dele, n, m)


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    events_path = Path(sys.argv[1])
    expected_path = Path(sys.argv[2])

    events = [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]
    expected = expected_path.read_text()

    finals = [e for e in events if e["type"] == "final"]
    partials = [e for e in events if e["type"] == "partial"]
    silences = [e for e in events if e["type"] == "silence_onset"]
    endpoints = [e for e in events if e["type"] == "endpoint"]

    hyp_text = " ".join(f["text"] for f in finals)
    werv, subs, ins, dele, ref_n, hyp_n = wer(expected, hyp_text)

    # Partial-latency proxy: wall_ms - audio_ms at emit.
    partial_lat = [p["wall_ms"] - p["audio_ms"] for p in partials]
    # Final latency similarly.
    final_lat = [f["wall_ms"] - f["audio_ms"] for f in finals]

    # Time-to-final: each silence_onset paired with the next final event (by wall_ms).
    ttf = []
    for s in silences:
        nxt = next((e for e in endpoints if e["wall_ms"] >= s["wall_ms"]), None)
        if nxt is not None:
            ttf.append(nxt["wall_ms"] - s["wall_ms"])

    def pct(xs, q):
        if not xs:
            return None
        s = sorted(xs)
        k = int(round((q / 100.0) * (len(s) - 1)))
        return s[k]

    print(f"events_path: {events_path}")
    print(f"total_events: {len(events)}")
    print(f"finals: {len(finals)}  partials: {len(partials)}  silence_onsets: {len(silences)}  endpoints: {len(endpoints)}")
    print()
    print(f"final_latency_p50_ms: {pct(final_lat, 50)}")
    print(f"final_latency_p95_ms: {pct(final_lat, 95)}")
    print(f"final_latency_max_ms: {max(final_lat) if final_lat else None}")
    print()
    print(f"partial_latency_p50_ms: {pct(partial_lat, 50)}")
    print(f"partial_latency_p95_ms: {pct(partial_lat, 95)}")
    print(f"partial_latency_max_ms: {max(partial_lat) if partial_lat else None}")
    print()
    print(f"time_to_final_count: {len(ttf)}")
    print(f"time_to_final_mean_ms: {statistics.mean(ttf) if ttf else None}")
    print(f"time_to_final_max_ms: {max(ttf) if ttf else None}")
    print()
    print(f"wer: {werv:.4f}  ({subs} sub, {ins} ins, {dele} del; ref_words={ref_n}, hyp_words={hyp_n})")
    print()
    print(f"hyp_text_sample (first 300 chars):")
    print(hyp_text[:300])


if __name__ == "__main__":
    main()
