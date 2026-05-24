#!/usr/bin/env python3
"""moonshine baseline harness (streaming) for #873.

Drives **streaming-trained Moonshine** (`moonshine-voice` package's
Transcriber class) at a configurable chunk cadence, paced to simulated
realtime, and emits a JSONL event log conforming to
[`baseline/src/events.rs`](../src/events.rs).

This is the **load-bearing measurement** for #873: does Moonshine —
which the paper claims is *trained* with sliding-window attention (a
streaming-native architecture, not an inference-time approximation) —
actually retain its offline WER under streaming, or does it drift in
the same way parakeet-mlx did (#872 measured ~3 pp drift per minute of
stream at `depth=1` because parakeet-tdt-0.6b-v2 is offline-trained)?

## Why not `mlx-audio`?

`mlx-audio` (the library used by [`run.py`](run.py)) does not expose a
streaming API for Moonshine — its `Model.generate()` is offline-only.
The streaming-trained variants live in the upstream
`moonshine-ai/moonshine` project and ship via `moonshine-voice`, which
exposes a `Transcriber.add_audio(chunk, sample_rate)` push API +
event-listener callbacks. `moonshine-voice` is ONNX-Runtime-backed
(CoreML execution provider on macOS), not MLX.

**Caveat:** unlike `run.py`, this path is *not* MLX/Metal-accelerated.
Absolute latency / RTF / peak RSS are therefore informational only and
not strictly comparable to MLX-accelerated baselines. The load-bearing
claim we measure here is the **streaming-vs-offline WER delta** — that
delta is runtime-independent and worth reporting even on a non-MLX
path.

## Pacing

The push loop sleeps so `wall_ms` catches up to `audio_ms` between
chunks (simulated realtime). This matches the pacing used in #826's
binaries and #856's parakeet streaming harness, so partial-latency P95
is measured the same way across baselines.

The Transcriber has an internal `update_interval` (default 0.5 s) that
governs how often it materialises a partial. We pass
`update_interval = chunk_ms / 1000` so partials are emitted at the same
cadence as the push loop, matching the spike's measurement convention.
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


def emit(log_fp, event: dict) -> None:
    log_fp.write(json.dumps(event) + "\n")
    log_fp.flush()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fixture", required=True, type=Path)
    ap.add_argument("--log", required=True, type=Path)
    ap.add_argument("--transcript", required=True, type=Path)
    ap.add_argument("--chunk-ms", type=int, default=100, help="Chunk cadence in milliseconds")
    ap.add_argument(
        "--model-arch",
        default="MEDIUM_STREAMING",
        help=(
            "moonshine-voice ModelArch enum name. One of: TINY_STREAMING, "
            "BASE_STREAMING, SMALL_STREAMING, MEDIUM_STREAMING. Default targets "
            "Medium Streaming (245 M params, 6.65 % WER per the paper)."
        ),
    )
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
    from moonshine_voice import ModelArch, Transcriber, TranscriptEventListener
    from moonshine_voice.download import download_model_from_info, find_model_info

    model_arch = ModelArch[args.model_arch]
    model_info = find_model_info(language="en", model_arch=model_arch)
    model_path, resolved_arch = download_model_from_info(model_info)
    transcriber = Transcriber(
        model_path=model_path,
        model_arch=resolved_arch,
        update_interval=args.chunk_ms / 1000.0,
    )
    load_ms = (time.monotonic_ns() - load_t0) // 1_000_000

    log_fp = args.log.open("w")
    emit(log_fp, {"type": "model_loaded", "wall_ms": wall_ms(), "load_ms": load_ms})

    state: dict = {
        "last_partial": "",
        "final_texts": [],
        "audio_pushed_ms": 0,
    }

    class EventEmitter(TranscriptEventListener):
        def on_line_text_changed(self, event) -> None:  # type: ignore[no-untyped-def]
            text = (event.line.text or "").strip()
            if text and text != state["last_partial"]:
                emit(
                    log_fp,
                    {
                        "type": "partial",
                        "wall_ms": wall_ms(),
                        "audio_ms": state["audio_pushed_ms"],
                        "text": text,
                    },
                )
                state["last_partial"] = text

        def on_line_completed(self, event) -> None:  # type: ignore[no-untyped-def]
            text = (event.line.text or "").strip()
            if text:
                emit(
                    log_fp,
                    {
                        "type": "final",
                        "wall_ms": wall_ms(),
                        "audio_ms": state["audio_pushed_ms"],
                        "event_id": str(ulid.ULID()),
                        "text": text,
                        "confidence": 1.0,
                    },
                )
                state["final_texts"].append(text)
                state["last_partial"] = ""

        def on_error(self, event) -> None:  # type: ignore[no-untyped-def]
            print(f"transcriber error: {event}", file=sys.stderr)

    transcriber.add_listener(EventEmitter())
    transcriber.start()

    try:
        for i in range(0, len(audio), chunk_samples):
            chunk = audio[i : i + chunk_samples]
            if len(chunk) == 0:
                break

            target_wall_ms = state["audio_pushed_ms"]
            actual_wall_ms = wall_ms()
            if actual_wall_ms < target_wall_ms:
                time.sleep((target_wall_ms - actual_wall_ms) / 1000.0)

            transcriber.add_audio(chunk.tolist(), sr)
            state["audio_pushed_ms"] += int(len(chunk) * 1000 / sr)
    finally:
        transcriber.stop()

    # Stream end: if the transcriber has unfinalised text in flight, promote
    # the last partial to a final so scripts/analyze.py can compute WER over
    # the entire stream rather than only completed lines.
    if state["last_partial"]:
        emit(
            log_fp,
            {
                "type": "final",
                "wall_ms": wall_ms(),
                "audio_ms": state["audio_pushed_ms"],
                "event_id": str(ulid.ULID()),
                "text": state["last_partial"],
                "confidence": 1.0,
            },
        )
        state["final_texts"].append(state["last_partial"])
        state["last_partial"] = ""

    emit(
        log_fp,
        {
            "type": "endpoint",
            "wall_ms": wall_ms(),
            "audio_ms": state["audio_pushed_ms"],
            "kind": "stream_end",
        },
    )
    emit(
        log_fp,
        {
            "type": "stream_end",
            "wall_ms": wall_ms(),
            "audio_ms": state["audio_pushed_ms"],
        },
    )
    log_fp.close()

    args.transcript.write_text(" ".join(state["final_texts"]).strip() + "\n")

    elapsed_ms = wall_ms()
    rtf = elapsed_ms / total_audio_ms if total_audio_ms else 0.0
    n_chunks = len(audio) // chunk_samples + (1 if len(audio) % chunk_samples else 0)
    print(
        f"chunks: {n_chunks}  "
        f"audio_ms: {total_audio_ms}  "
        f"wall_ms: {elapsed_ms}  "
        f"RTF (wall/audio): {rtf:.3f}  "
        f"load_ms: {load_ms}  "
        f"finals: {len(state['final_texts'])}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
