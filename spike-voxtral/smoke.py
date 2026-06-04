#!/usr/bin/env python3
"""Download the Voxtral Realtime 4-bit checkpoint and run two sanity probes.

1. Batch transcription of a short head slice — confirms the model loads and
   emits sane English, and gives the *start anchor* words used to trim the
   canonical Gutenberg reference to the fixture's spoken span.
2. A short streaming session at the default 480 ms delay — confirms the
   feed()/step() API yields text deltas before we build the full sweep.

Run: spike-voxtral/.venv/bin/python spike-voxtral/smoke.py
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

import numpy as np
import soundfile as sf

from mlx_audio.stt.utils import load_model

MODEL = "mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit"
FIXTURE = Path(__file__).resolve().parents[1] / "tests/fixtures/voice/monologue_5min.wav"


def main() -> int:
    print(f"loading {MODEL} (downloads ~3 GB on first run) ...", flush=True)
    t0 = time.monotonic()
    model = load_model(MODEL)
    print(f"  loaded in {time.monotonic() - t0:.1f}s", flush=True)

    audio, sr = sf.read(FIXTURE, dtype="float32")
    if sr != 16000 or audio.ndim != 1:
        sys.exit(f"expected 16 kHz mono, got sr={sr} ndim={audio.ndim}")
    print(f"fixture: {len(audio)} samples = {len(audio)/sr:.1f}s @ {sr} Hz", flush=True)

    head = audio[: sr * 20]

    print("\n=== batch generate() on first 20 s ===", flush=True)
    t0 = time.monotonic()
    out = model.generate(head, temperature=0.0)
    print(f"  ({time.monotonic()-t0:.1f}s)\n  {out.text!r}", flush=True)

    print("\n=== streaming session (480 ms) on first 20 s, 80 ms feeds ===", flush=True)
    sess = model.create_streaming_session(transcription_delay_ms=480, temperature=0.0)
    chunk = sr * 80 // 1000
    deltas = []
    for i in range(0, len(head), chunk):
        sess.feed(head[i : i + chunk])
        for d in sess.step(max_decode_tokens=8):
            deltas.append(d)
    sess.close()
    while not sess.done:
        for d in sess.step(max_decode_tokens=8):
            deltas.append(d)
    print(f"  deltas={len(deltas)}\n  {''.join(deltas)!r}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
