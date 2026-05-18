#!/usr/bin/env python3
"""parakeet-mlx baseline harness for #856.

Loads NVIDIA Parakeet-TDT-0.6B-v2 via parakeet-mlx, transcribes the
committed 5-min fixture, and emits a JSONL event log conforming to
baseline/src/events.rs from #826.

NOT a candidate; NOT a runtime path. Reference quality baseline only.

The harness uses the utterance-level `model.transcribe(path)` API rather
than `model.transcribe_stream(...)`. The streaming API exists but a
100 ms-chunked feed with default context_size=(256, 256), depth=1
produces unusable output on this fixture (model lacks sufficient context
per chunk to commit to good hypotheses; see SPIKE.md). The
issue (#856) explicitly permits this fallback:

>   if the `parakeet-mlx` API only exposes utterance-level transcription
>   (document and skip partial-latency comparison rather than fabricate one)

Consequently, `partial` events and `silence_onset`/`endpoint{silence_gap}`
events are NOT emitted; partial-latency and time-to-final are reported
as N/A.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import soundfile as sf
import ulid
from parakeet_mlx import from_pretrained
from parakeet_mlx.alignment import AlignedResult

SAMPLE_RATE = 16_000
DEFAULT_MODEL = "mlx-community/parakeet-tdt-0.6b-v2"


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

    # Validate fixture shape so a wrong path or mismatched sample rate fails fast
    # rather than producing silent garbage downstream.
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
    model = from_pretrained(args.model)
    load_ms = (time.monotonic_ns() - load_t0) // 1_000_000

    log_fp = args.log.open("w")
    emit(log_fp, {"type": "model_loaded", "wall_ms": wall_ms(), "load_ms": load_ms})

    inference_start_wall_ms = wall_ms()
    result: AlignedResult = model.transcribe(args.fixture)
    inference_end_wall_ms = wall_ms()
    inference_ms = inference_end_wall_ms - inference_start_wall_ms

    # Emit one `final` per aligned sentence. `audio_ms` is the sentence's end
    # timestamp in the original audio; `wall_ms` is identical across all finals
    # because they were all produced in a single utterance-level transcribe call.
    for sent in result.sentences:
        emit(
            log_fp,
            {
                "type": "final",
                "wall_ms": inference_end_wall_ms,
                "audio_ms": int(sent.end * 1000),
                "event_id": str(ulid.ULID()),
                "text": sent.text,
                "confidence": float(sent.confidence),
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

    args.transcript.write_text(result.text.strip() + "\n")

    rtf = inference_ms / total_audio_ms if total_audio_ms else 0.0
    print(
        f"finals: {len(result.sentences)}  "
        f"audio_ms: {total_audio_ms}  "
        f"inference_ms: {inference_ms}  "
        f"RTF (inference/audio): {rtf:.3f}  "
        f"load_ms: {load_ms}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
