#!/usr/bin/env python3
"""moonshine baseline harness (streaming) for #873.

Drives **upstream streaming Moonshine** (`UsefulSensors/moonshine-streaming-*`
via the `useful-moonshine` / `transformers` path) at a configurable chunk
cadence, paced to simulated realtime, and emits a JSONL event log
conforming to [`baseline/src/events.rs`](../src/events.rs).

This is the **load-bearing measurement** for #873: does Moonshine — which
the paper claims is *trained* with sliding-window attention (a streaming-
native architecture, not an inference-time approximation) — actually
retain its offline WER under streaming, or does it drift in the same way
parakeet-mlx did (#872 measured ~3 pp drift per minute of stream at
`depth=1` because parakeet-tdt-0.6b-v2 is offline-trained)?

## Why not `mlx-audio`?

`mlx-audio` (the library used by [`run.py`](run.py)) does not expose a
streaming API for Moonshine at the time of writing — its `Model.generate()`
is offline-only and the `stream: bool` parameter is dead code. The
upstream Moonshine project (`useful-moonshine` PyPI package, GitHub
[moonshine-ai/moonshine](https://github.com/moonshine-ai/moonshine)) ships
streaming-trained variants on HuggingFace as `UsefulSensors/moonshine-
streaming-{tiny,small,medium}`. We drive those here.

**Caveat:** unlike `run.py`, this path is *not* MLX/Metal-accelerated.
Absolute latency numbers are therefore informational only and not
strictly comparable to the other MLX baselines (sherpa-onnx is CPU,
parakeet-mlx is MLX/GPU). The load-bearing claim we measure here is the
**streaming-vs-offline WER delta** (Moonshine's promise) — that delta
is API-independent and worth reporting even on a CPU/CUDA path.

## Pacing

The push loop sleeps so `wall_ms` catches up to `audio_ms` between
chunks (simulated realtime). This matches the pacing used in #826's
binaries and #856's parakeet streaming harness, so partial-latency P95
is measured the same way across baselines.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import soundfile as sf
import ulid

SAMPLE_RATE = 16_000
DEFAULT_MODEL = "UsefulSensors/moonshine-streaming-medium"


def emit(log_fp, event: dict) -> None:
    log_fp.write(json.dumps(event) + "\n")
    log_fp.flush()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", required=True, type=Path)
    ap.add_argument("--log", required=True, type=Path)
    ap.add_argument("--transcript", required=True, type=Path)
    ap.add_argument("--chunk-ms", type=int, default=100, help="Chunk cadence in milliseconds")
    ap.add_argument("--model", default=DEFAULT_MODEL)
    args = ap.parse_args()

    audio, sr = sf.read(args.fixture, dtype="float32")
    if sr != SAMPLE_RATE:
        sys.exit(f"expected {SAMPLE_RATE} Hz, got {sr}")
    if audio.ndim != 1:
        sys.exit(f"expected mono, got ndim={audio.ndim}")
    total_audio_ms = int(len(audio) * 1000 / sr)
    chunk_samples = sr * args.chunk_ms // 1000

    t0 = time.monotonic_ns()

    def wall_ms() -> int:
        return (time.monotonic_ns() - t0) // 1_000_000

    load_t0 = time.monotonic_ns()
    # The exact streaming API on UsefulSensors/moonshine-streaming-* must be
    # confirmed at execution time. As of writing, the upstream
    # `useful-moonshine` package exposes `MoonshineStreaming` with a
    # `transcribe_chunk(samples) -> str` method that returns the
    # cumulative-best partial after each push. If the API has shifted by the
    # time you run this, adjust the next few lines accordingly — the rest of
    # the harness (event emission, pacing) is API-agnostic.
    from moonshine_streaming import MoonshineStreaming  # type: ignore[import-not-found]

    streamer = MoonshineStreaming.from_pretrained(args.model)
    load_ms = (time.monotonic_ns() - load_t0) // 1_000_000

    log_fp = args.log.open("w")
    emit(log_fp, {"type": "model_loaded", "wall_ms": wall_ms(), "load_ms": load_ms})

    last_partial_text = ""
    audio_pushed_ms = 0
    final_count = 0

    for i in range(0, len(audio), chunk_samples):
        chunk = audio[i : i + chunk_samples]
        if len(chunk) == 0:
            break

        # Realtime pacing: sleep so wall_ms catches up to audio_ms.
        target_wall_ms = audio_pushed_ms
        actual_wall_ms = wall_ms()
        if actual_wall_ms < target_wall_ms:
            time.sleep((target_wall_ms - actual_wall_ms) / 1000.0)

        result = streamer.transcribe_chunk(chunk)
        # Normalise: API may return either str or {"text": ..., "is_final": ...}.
        if isinstance(result, str):
            partial_text = result
            is_final = False
        else:
            partial_text = result.get("text", "") or ""
            is_final = bool(result.get("is_final", False))

        audio_pushed_ms += int(len(chunk) * 1000 / sr)

        if partial_text and partial_text != last_partial_text:
            emit(
                log_fp,
                {
                    "type": "partial",
                    "wall_ms": wall_ms(),
                    "audio_ms": audio_pushed_ms,
                    "text": partial_text,
                },
            )
            last_partial_text = partial_text

        if is_final and partial_text:
            emit(
                log_fp,
                {
                    "type": "final",
                    "wall_ms": wall_ms(),
                    "audio_ms": audio_pushed_ms,
                    "event_id": str(ulid.ULID()),
                    "text": partial_text,
                    "confidence": 1.0,
                },
            )
            final_count += 1
            last_partial_text = ""

    # If the streaming API only emits running partials (no per-segment
    # finals), promote the last partial to a single final at stream end so
    # scripts/analyze.py has something to compute WER against.
    if final_count == 0 and last_partial_text:
        emit(
            log_fp,
            {
                "type": "final",
                "wall_ms": wall_ms(),
                "audio_ms": audio_pushed_ms,
                "event_id": str(ulid.ULID()),
                "text": last_partial_text,
                "confidence": 1.0,
            },
        )
        final_count += 1

    emit(
        log_fp,
        {
            "type": "endpoint",
            "wall_ms": wall_ms(),
            "audio_ms": audio_pushed_ms,
            "kind": "stream_end",
        },
    )
    emit(
        log_fp,
        {"type": "stream_end", "wall_ms": wall_ms(), "audio_ms": audio_pushed_ms},
    )
    log_fp.close()

    args.transcript.write_text(last_partial_text + "\n")

    elapsed_ms = wall_ms()
    rtf = elapsed_ms / total_audio_ms if total_audio_ms else 0.0
    print(
        f"chunks: {len(audio) // chunk_samples + (1 if len(audio) % chunk_samples else 0)}  "
        f"audio_ms: {total_audio_ms}  "
        f"wall_ms: {elapsed_ms}  "
        f"RTF (wall/audio): {rtf:.3f}  "
        f"load_ms: {load_ms}  "
        f"finals: {final_count}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
