#!/usr/bin/env python3
"""moonshine baseline harness (offline) for #873.

Loads Moonshine via `mlx-audio` and transcribes the committed 5-min
fixture in fixed-length chunks (Moonshine's encoder has
`max_position_embeddings = 512`, training data was ≤ 30 s utterances —
feeding 5 min in one call produces empty output). Per-chunk transcripts
are concatenated and emitted as a single `final` event in the JSONL log
conforming to [`baseline/src/events.rs`](../src/events.rs).

NOT a candidate; NOT a runtime path. Reference WER baseline only — the
question this harness answers is "what is Moonshine's offline WER on the
same fixture parakeet-mlx (#856) and sherpa-onnx (#859) were measured
on?". The streaming-WER question (Moonshine's load-bearing claim of
streaming-vs-offline parity) is answered by the sibling harness
[`run_streaming.py`](run_streaming.py), which uses the
[`moonshine-voice`](https://pypi.org/project/moonshine-voice/)
streaming Transcriber.

Because this is offline, `partial` events and
`silence_onset`/`endpoint{silence_gap}` events are NOT emitted; partial
latency and time-to-final are reported as N/A — matching the
parakeet-mlx (#856) fallback pattern.

Chunking note: the published Moonshine demos chunk at 30 s with VAD-
driven boundaries. We use fixed 25-second windows for determinism and
simplicity. This may give up a small amount of WER vs VAD-boundary
chunking (a chunk boundary mid-word can clip a token), but it's
deterministic across runs and simpler to compare across baselines.
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
DEFAULT_MODEL = "UsefulSensors/moonshine-base"
DEFAULT_CHUNK_SECONDS = 25


def emit(log_fp, event: dict) -> None:
    log_fp.write(json.dumps(event) + "\n")
    log_fp.flush()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", required=True, type=Path)
    ap.add_argument("--log", required=True, type=Path)
    ap.add_argument("--transcript", required=True, type=Path)
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument(
        "--chunk-seconds",
        type=int,
        default=DEFAULT_CHUNK_SECONDS,
        help="Audio chunk length for the encoder (Moonshine trained on ≤ 30 s)",
    )
    args = ap.parse_args()

    audio, sr = sf.read(args.fixture, dtype="float32")
    if sr != SAMPLE_RATE:
        sys.exit(f"expected {SAMPLE_RATE} Hz, got {sr}")
    if audio.ndim != 1:
        sys.exit(f"expected mono, got ndim={audio.ndim}")
    total_audio_ms = int(len(audio) * 1000 / sr)
    chunk_samples = sr * args.chunk_seconds

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
    chunk_texts: list[str] = []
    for i in range(0, len(audio), chunk_samples):
        chunk = audio[i : i + chunk_samples]
        if len(chunk) == 0:
            break
        result = model.generate(chunk)
        chunk_text = (getattr(result, "text", "") or "").strip()
        if chunk_text:
            chunk_texts.append(chunk_text)
    inference_end_wall_ms = wall_ms()
    inference_ms = inference_end_wall_ms - inference_start_wall_ms

    text = " ".join(chunk_texts).strip()

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
        f"chunks: {len(chunk_texts)}  "
        f"audio_ms: {total_audio_ms}  "
        f"inference_ms: {inference_ms}  "
        f"RTF (inference/audio): {rtf:.3f}  "
        f"load_ms: {load_ms}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
