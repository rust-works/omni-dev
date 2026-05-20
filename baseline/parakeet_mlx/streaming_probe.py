#!/usr/bin/env python3
"""Reproducible probe of parakeet-mlx's streaming API for #856.

Resolves the open question "does the streaming API produce usable partials?"
by sweeping (chunk_ms, context_size, depth) on a 10 s smoke slice of the
#826 fixture and printing the resulting transcript next to the ground truth.

Empirical finding (documented in SPIKE.md):
  - At chunk_ms=100 (#826's production cadence), output is unusable
    garbage regardless of depth (1, 4, 24) or context_size ((256,256),
    (512,512)). All produce the same wrong tokens, ruling out
    cache-divergence as the cause.
  - At chunk_ms>=500, output matches the non-streaming transcribe()
    result and RTF stays well under realtime.

Conclusion: partial-latency and time-to-final are N/A *for the 100 ms
chunk cadence required by #826*. The streaming API itself is usable at
>=500 ms chunks; we just can't honestly emit those metrics for the
target use case.
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

import mlx.core as mx
import soundfile as sf
from parakeet_mlx import from_pretrained
from parakeet_mlx.parakeet import DecodingConfig, SentenceConfig

DEFAULT_FIXTURE = Path(__file__).parent.parent.parent / "tests" / "fixtures" / "voice" / "monologue_5min.wav"


def main() -> int:
    import argparse

    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", type=Path, default=DEFAULT_FIXTURE)
    ap.add_argument("--seconds", type=int, default=10, help="Slice length from the start of the fixture")
    ap.add_argument("--model", default="mlx-community/parakeet-tdt-0.6b-v2")
    args = ap.parse_args()

    audio, sr = sf.read(args.fixture, dtype="float32")
    if sr != 16_000 or audio.ndim != 1:
        sys.exit(f"expected 16 kHz mono, got sr={sr} ndim={audio.ndim}")
    audio = audio[: sr * args.seconds]

    m = from_pretrained(args.model)
    cfg = DecodingConfig(sentence=SentenceConfig(silence_gap=0.5))

    configs = [
        (100, (256, 256), 1),
        (100, (256, 256), 4),
        (100, (256, 256), 24),
        (100, (512, 512), 24),
        (500, (256, 256), 1),
        (500, (256, 256), 24),
        (1000, (256, 256), 1),
        (1000, (256, 256), 24),
    ]

    print(f"slice: {args.seconds}s  ({len(audio)} samples @ {sr} Hz)\n")
    for chunk_ms, ctx, depth in configs:
        chunk_samples = sr * chunk_ms // 1000
        with m.transcribe_stream(context_size=ctx, depth=depth, decoding_config=cfg) as s:
            t0 = time.monotonic()
            for i in range(0, len(audio), chunk_samples):
                s.add_audio(mx.array(audio[i : i + chunk_samples]))
            elapsed = time.monotonic() - t0
            text = s.result.text.strip()
        rtf = elapsed / (len(audio) / sr)
        print(f"chunk_ms={chunk_ms:>4}  ctx={ctx}  depth={depth:>2}  RTF={rtf:.2f}")
        print(f"  {text[:200]!r}\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
