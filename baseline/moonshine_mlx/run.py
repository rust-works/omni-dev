#!/usr/bin/env python3
"""moonshine baseline harness (offline) for #873.

Loads Moonshine via `mlx-audio`, transcribes the committed 5-min fixture
in one call, and emits a JSONL event log conforming to
[`baseline/src/events.rs`](../src/events.rs).

NOT a candidate; NOT a runtime path. Reference WER baseline only — the
question this harness answers is "what is Moonshine's offline WER on the
same fixture parakeet-mlx (#856) and sherpa-onnx (#859) were measured
on?". The streaming-WER question (Moonshine's load-bearing claim of
streaming-vs-offline parity) is answered by the sibling harness
[`run_streaming.py`](run_streaming.py), which uses the upstream
`useful-transformers` / `transformers` path because `mlx-audio` does
not expose a streaming API for Moonshine at the time of writing.

Because this is offline, `partial` events and
`silence_onset`/`endpoint{silence_gap}` events are NOT emitted; partial
latency and time-to-final are reported as N/A — matching the
parakeet-mlx (#856) fallback pattern.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import soundfile as sf
import ulid

SAMPLE_RATE = 16_000
DEFAULT_MODEL = "mlx-community/moonshine-base"


def emit(log_fp, event: dict) -> None:
    log_fp.write(json.dumps(event) + "\n")
    log_fp.flush()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", required=True, type=Path)
    ap.add_argument("--log", required=True, type=Path)
    ap.add_argument("--transcript", required=True, type=Path)
    ap.add_argument("--model", default=DEFAULT_MODEL)
    args = ap.parse_args()

    info = sf.info(args.fixture)
    if info.samplerate != SAMPLE_RATE:
        sys.exit(f"expected {SAMPLE_RATE} Hz, got {info.samplerate}")
    if info.channels != 1:
        sys.exit(f"expected mono, got {info.channels} channels")
    total_audio_ms = int(info.frames * 1000 / info.samplerate)

    t0 = time.monotonic_ns()

    def wall_ms() -> int:
        return (time.monotonic_ns() - t0) // 1_000_000

    load_t0 = time.monotonic_ns()
    from mlx_audio.stt import load_model

    model = load_model(args.model)
    load_ms = (time.monotonic_ns() - load_t0) // 1_000_000

    log_fp = args.log.open("w")
    emit(log_fp, {"type": "model_loaded", "wall_ms": wall_ms(), "load_ms": load_ms})

    inference_start_wall_ms = wall_ms()
    result = model.generate(str(args.fixture))
    inference_end_wall_ms = wall_ms()
    inference_ms = inference_end_wall_ms - inference_start_wall_ms

    text = (getattr(result, "text", None) or str(result)).strip()

    emit(
        log_fp,
        {
            "type": "final",
            "wall_ms": inference_end_wall_ms,
            "audio_ms": total_audio_ms,
            "event_id": str(ulid.ULID()),
            "text": text,
            "confidence": 1.0,
        },
    )
    emit(
        log_fp,
        {
            "type": "endpoint",
            "wall_ms": wall_ms(),
            "audio_ms": total_audio_ms,
            "kind": "stream_end",
        },
    )
    emit(
        log_fp,
        {"type": "stream_end", "wall_ms": wall_ms(), "audio_ms": total_audio_ms},
    )
    log_fp.close()

    args.transcript.write_text(text + "\n")

    rtf = inference_ms / total_audio_ms if total_audio_ms else 0.0
    print(
        f"audio_ms: {total_audio_ms}  "
        f"inference_ms: {inference_ms}  "
        f"RTF (inference/audio): {rtf:.3f}  "
        f"load_ms: {load_ms}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
