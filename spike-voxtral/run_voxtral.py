#!/usr/bin/env python3
"""Streaming-ASR latency/accuracy/memory harness for Voxtral Realtime (#930).

Drives ``mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit`` via mlx-audio's
``VoxtralStreamingSession`` over the committed 5-min Sherlock fixture, sweeping
the ``transcription_delay_ms`` knob. For each delay it writes:

  results/events_<delay>.jsonl   one JSON line per emitted Partial + a summary
  results/transcript_<delay>.txt the concatenated streaming transcript

LATENCY PROXY HONESTY NOTE (mirrors #856's framing):
  Audio is fed in real-time-paced 80 ms chunks (chunk i is withheld until
  wall-clock t0 + i*80 ms), so each Partial's ``wall_ms`` is measured from
  "audio starts" (t0). ``first_partial_wall_ms`` is therefore the real
  end-to-end latency from stream start to the first non-empty Partial,
  INCLUDING the model's intrinsic ``transcription_delay_ms`` audio-accumulation
  lag. It is NOT measured against word-level ground-truth timestamps; absolute
  latency to a *spoken word* would differ by the (unknown) offset between the
  fixture's t0 and the first spoken word. The cross-delay comparison is honest
  because every run is paced identically.

  RTF is pacing-independent: it is sum(time spent inside step()) / audio_secs,
  i.e. the pure compute fraction. ``max_feed_lag_ms`` reports how far behind
  real-time the feed loop fell (driven by step() compute) — >~1 chunk sustained
  means the model cannot keep up with live input at that delay.

Usage:
  spike-voxtral/.venv/bin/python spike-voxtral/run_voxtral.py \
      --delays 80 240 480 1000 --out spike-voxtral/results
  # quick smoke: --seconds 20 --delays 480
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import soundfile as sf

from mlx_audio.stt.models.voxtral_realtime.config import _num_delay_tokens
from mlx_audio.stt.utils import load_model

MODEL = "mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit"
FIXTURE = Path(__file__).resolve().parents[1] / "tests/fixtures/voice/monologue_5min.wav"
SR = 16000
CHUNK_MS = 80  # feed cadence; one RAW_AUDIO_LENGTH_PER_TOK = 1280 samples = 80 ms


def run_one(model, audio: np.ndarray, delay_ms: int, out_dir: Path, pace: bool) -> dict:
    """Run a single paced streaming session at the given delay; return summary."""
    sess = model.create_streaming_session(transcription_delay_ms=delay_ms, temperature=0.0)
    chunk = SR * CHUNK_MS // 1000
    n = len(audio)
    audio_secs = n / SR

    events_path = out_dir / f"events_{delay_ms}.jsonl"
    ev = events_path.open("w")

    deltas: list[str] = []
    first_partial_wall_ms = None
    step_time_total = 0.0
    max_feed_lag_ms = 0.0

    def drain(audio_ms: float):
        nonlocal first_partial_wall_ms, step_time_total
        s0 = time.monotonic()
        ds = sess.step(max_decode_tokens=16)
        step_time_total += time.monotonic() - s0
        for d in ds:
            wall_ms = (time.monotonic() - t0) * 1000.0
            deltas.append(d)
            if first_partial_wall_ms is None and d.strip():
                first_partial_wall_ms = wall_ms
            ev.write(json.dumps({
                "type": "partial", "wall_ms": round(wall_ms, 1),
                "audio_ms": round(audio_ms, 1), "text_delta": d,
                "cum_chars": sum(len(x) for x in deltas),
            }) + "\n")

    t0 = time.monotonic()
    for i in range(0, n, chunk):
        if pace:
            target = t0 + (i / SR)
            now = time.monotonic()
            if now < target:
                time.sleep(target - now)
            else:
                max_feed_lag_ms = max(max_feed_lag_ms, (now - target) * 1000.0)
        sess.feed(audio[i : i + chunk])
        drain(audio_ms=min((i + chunk) / SR, audio_secs) * 1000.0)

    close_wall_ms = (time.monotonic() - t0) * 1000.0
    sess.close()
    while not sess.done:
        drain(audio_ms=audio_secs * 1000.0)
    done_wall_ms = (time.monotonic() - t0) * 1000.0

    transcript = "".join(deltas).strip()
    (out_dir / f"transcript_{delay_ms}.txt").write_text(transcript + "\n", encoding="utf-8")

    summary = {
        "type": "summary",
        "delay_ms": delay_ms,
        "effective_delay_tokens": _num_delay_tokens(delay_ms),
        "effective_delay_ms": _num_delay_tokens(delay_ms) * CHUNK_MS,
        "first_partial_wall_ms": round(first_partial_wall_ms, 1) if first_partial_wall_ms else None,
        "close_wall_ms": round(close_wall_ms, 1),
        "done_wall_ms": round(done_wall_ms, 1),
        "end_of_utterance_ms": round(done_wall_ms - close_wall_ms, 1),
        "audio_ms_total": round(audio_secs * 1000.0, 1),
        "step_time_total_ms": round(step_time_total * 1000.0, 1),
        "rtf": round(step_time_total / audio_secs, 4),
        "max_feed_lag_ms": round(max_feed_lag_ms, 1),
        "paced": pace,
        "n_deltas": len(deltas),
        "n_tokens": len(sess.generated),
        "transcript_chars": len(transcript),
    }
    ev.write(json.dumps(summary) + "\n")
    ev.close()
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", type=Path, default=FIXTURE)
    ap.add_argument("--delays", type=int, nargs="+", default=[80, 240, 480, 1000])
    ap.add_argument("--seconds", type=int, default=0, help="0 = full fixture; else head slice")
    ap.add_argument("--out", type=Path, default=Path(__file__).parent / "results")
    ap.add_argument("--no-pace", action="store_true", help="feed as fast as possible (RTF/mem run)")
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    audio, sr = sf.read(args.fixture, dtype="float32")
    if sr != SR or audio.ndim != 1:
        sys.exit(f"expected 16 kHz mono, got sr={sr} ndim={audio.ndim}")
    if args.seconds:
        audio = audio[: sr * args.seconds]

    print(f"loading {MODEL} ...", flush=True)
    t0 = time.monotonic()
    model = load_model(MODEL)
    print(f"  loaded in {time.monotonic()-t0:.1f}s; audio={len(audio)/sr:.1f}s "
          f"pace={'no' if args.no_pace else 'realtime'}", flush=True)

    summaries = []
    for d in args.delays:
        print(f"\n=== delay {d} ms ===", flush=True)
        s = run_one(model, audio, d, args.out, pace=not args.no_pace)
        summaries.append(s)
        print(f"  first_partial={s['first_partial_wall_ms']}ms  eou={s['end_of_utterance_ms']}ms  "
              f"rtf={s['rtf']}  lag={s['max_feed_lag_ms']}ms  tokens={s['n_tokens']}", flush=True)

    (args.out / "summaries.json").write_text(json.dumps(summaries, indent=2), encoding="utf-8")
    print(f"\nwrote {args.out}/summaries.json", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
