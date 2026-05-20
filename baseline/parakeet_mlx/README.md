# `parakeet-mlx` reference baseline (#856)

One-off harness measuring **NVIDIA Parakeet-TDT-0.6B-v2** (via the
[`parakeet-mlx`](https://pypi.org/project/parakeet-mlx/) Python package)
as a **reference WER ceiling** for the candle-vs-tract decision in
[#826](https://github.com/rust-works/omni-dev/issues/826).

**This is not a runtime candidate.** Not a Rust port. Not a production
path. The output is a number and a paragraph of judgement appended to
the spike's [`SPIKE.md`](../../SPIKE.md). See
[the issue](https://github.com/rust-works/omni-dev/issues/856) for full
scope.

## Host requirements

- macOS / Apple Silicon (MLX is Apple-only).
- Python 3.10–3.12 (3.14 is too new for the MLX wheels at time of
  writing; tested on 3.12.12).
- Network access to HuggingFace for the one-time model fetch (~1.2 GB).

## Setup

```bash
# from baseline/parakeet_mlx/
python3.12 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

(The plan called for `uv venv`; standard `venv` is used because `uv`
isn't required on the spike host. `requirements.txt` is `pip`-/`uv`-
compatible.)

## Run

```bash
source .venv/bin/activate
/usr/bin/time -l python run.py \
    --fixture ../../tests/fixtures/voice/monologue_5min.wav \
    --log events.jsonl \
    --transcript transcript.txt \
    2>peak_rss.txt

python ../../scripts/analyze.py events.jsonl \
    ../../tests/fixtures/voice/monologue_5min.expected.txt
```

Both `events.jsonl` and `transcript.txt` are gitignored — they're
regenerated per-run. `transcript.txt` is hash-equal across runs;
`events.jsonl` differs only in `event_id` ULIDs and wall-clock
timestamps.

## What the harness does (and doesn't) measure

`run.py` calls `model.transcribe(path)` — the **utterance-level** API
— and emits one `final` JSONL event per `AlignedSentence`. The
emitted shape conforms to
[`baseline/src/events.rs`](../src/events.rs) so the existing
[`scripts/analyze.py`](../../scripts/analyze.py) consumes it
unchanged.

The package also exposes a streaming API (`model.transcribe_stream`).
We probed it ([`streaming_probe.py`](streaming_probe.py)) sweeping
`chunk_ms ∈ {100, 500, 1000}` × `depth ∈ {1, 4, 24}` × `context_size ∈
{(256,256), (512,512)}`. **Finding: usable at chunks ≥ 500 ms; broken
at 100 ms** (same byte-identical garbage across all depth and
context_size settings; not a cache-divergence problem). Since #826's
production cadence is 100 ms (matching `cpal`'s capture callback), we
report `partial_latency` and `time_to_final` as N/A and use the
utterance-level path. Per the [#856 issue text](https://github.com/rust-works/omni-dev/issues/856)'s
explicit fallback: *"if the parakeet-mlx API only exposes
utterance-level transcription, document and skip partial-latency
comparison rather than fabricate one"*.

## Files

| File | Tracked | Purpose |
|---|---|---|
| `run.py` | yes | The harness (~110 lines) |
| `requirements.txt` | yes | Pinned deps (`parakeet-mlx==0.5.1`) |
| `README.md` | yes | This file |
| `.venv/` | **gitignored** | uv/venv-managed Python env |
| `events.jsonl` | **gitignored** | Per-run JSONL output |
| `transcript.txt` | **gitignored** | Per-run transcript |
| `metrics.txt`, `peak_rss.txt` | **gitignored** | Analysis dump + `time -l` capture |
