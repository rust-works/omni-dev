# `moonshine` reference baseline (#873)

One-off harness measuring **Moonshine v2** against the same committed
5-min fixture used by [`parakeet_mlx`](../parakeet_mlx/) (#856),
`sherpa_onnx` (#859), and the candle path (#826). The deliverable is a
row in [`COMPARISON.md`](../../COMPARISON.md) and an implication
paragraph — **not** a runtime path, **not** an ADR.

The load-bearing question this harness answers:

> Moonshine's paper claims sliding-window attention is its *training-time*
> attention pattern, not an inference-time approximation. If that holds,
> streaming and offline WER should match on this fixture — unlike
> parakeet-mlx, where #872 measured ~3 pp WER drift per minute of stream
> at the default streaming-API depth.

If it holds, Moonshine sits between sherpa-onnx's WER and parakeet-mlx's,
*and* preserves sherpa's streaming-latency characteristic, on Apple
Silicon.

## Host requirements

- macOS / Apple Silicon (the `run.py` path uses MLX; `run_streaming.py`
  is CPU/CUDA via the upstream package and runs cross-platform, but the
  whole spike's framing is Apple-only).
- Python 3.10–3.12 (3.14 is too new for MLX wheels at time of writing).
- Network access to HuggingFace for one-time model fetches.

## Dual-backend rationale

`mlx-audio` only exposes Moonshine offline — its `Model.generate()` does
the whole audio in one call, and the `stream: bool` parameter is dead
code. The streaming-trained variants
(`UsefulSensors/moonshine-streaming-{tiny,small,medium}`) live in the
upstream `moonshine-ai/moonshine` project and are driven via the
`useful-moonshine` package (or `transformers` directly), not `mlx-audio`.

So we split:

| File | Backend | Measures |
|---|---|---|
| `run.py` | `mlx-audio` (MLX/Metal) | Offline WER + load + RTF + peak RSS |
| `run_streaming.py` | `useful-moonshine` / `transformers` (CPU/CUDA) | Streaming WER at 100/500 ms cadence; streaming-vs-offline delta; partial latency P50/P95 |

**Important caveat:** the streaming path is *not* MLX-accelerated. Its
absolute latency / RTF / peak RSS are informational only and not directly
comparable to the other MLX baselines. The streaming-vs-offline **WER
delta** (Moonshine's load-bearing claim) is API-independent — that
remains the headline number.

## Setup

```bash
# from baseline/moonshine_mlx/
python3.12 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

Pins live in [`requirements.txt`](requirements.txt). They start as
version floors; replace with exact versions after the first clean
install on the spike host (the standard pattern used by sibling
baselines).

If `useful-moonshine` does not install cleanly or the streaming variants
do not resolve on HuggingFace, see the **Abandon path** section below.

## Run

```bash
source .venv/bin/activate

# Offline (mlx-audio, MLX/GPU) — the WER ceiling on this fixture.
/usr/bin/time -l python run.py \
    --fixture ../../tests/fixtures/voice/monologue_5min.wav \
    --log events_offline.jsonl \
    --transcript transcript_offline.txt \
    2>peak_rss_offline.txt

# Streaming at 100 ms (the load-bearing measurement: does Moonshine
# actually retain offline quality when streaming?).
/usr/bin/time -l python run_streaming.py \
    --fixture ../../tests/fixtures/voice/monologue_5min.wav \
    --log events_streaming_100ms.jsonl \
    --transcript transcript_streaming_100ms.txt \
    --chunk-ms 100 \
    2>peak_rss_streaming_100ms.txt

# Streaming at 500 ms (apples-to-apples cadence vs sherpa-onnx's ~450 ms
# native cadence and #856's parakeet-mlx 500 ms point).
/usr/bin/time -l python run_streaming.py \
    --fixture ../../tests/fixtures/voice/monologue_5min.wav \
    --log events_streaming_500ms.jsonl \
    --transcript transcript_streaming_500ms.txt \
    --chunk-ms 500 \
    2>peak_rss_streaming_500ms.txt

# Analysis (same script across all baselines).
python ../../scripts/analyze.py events_offline.jsonl \
    ../../tests/fixtures/voice/monologue_5min.expected.txt
python ../../scripts/analyze.py events_streaming_100ms.jsonl \
    ../../tests/fixtures/voice/monologue_5min.expected.txt
python ../../scripts/analyze.py events_streaming_500ms.jsonl \
    ../../tests/fixtures/voice/monologue_5min.expected.txt
```

## Determinism check

Two consecutive runs of `run.py` must produce a byte-equal
`transcript_offline.txt`:

```bash
python run.py --fixture ../../tests/fixtures/voice/monologue_5min.wav \
    --log /tmp/m1.jsonl --transcript /tmp/m1.txt
python run.py --fixture ../../tests/fixtures/voice/monologue_5min.wav \
    --log /tmp/m2.jsonl --transcript /tmp/m2.txt
diff /tmp/m1.txt /tmp/m2.txt  # must be empty
```

(The streaming harness output may differ by `event_id` ULIDs and
wall-clock timestamps in `events_*.jsonl`, but `transcript_*.txt`
content should be byte-equal across runs.)

## Time-box

**Hard 1-day cap** per the issue. If install or model fetch fails by the
end of the first hour, follow the **Abandon path** below — the same
fallback #856's plan used.

## Abandon path

If the spike abandons (install fails, model unavailable, both backends
broken), document the failure in [`COMPARISON.md`](../../COMPARISON.md)
as a brief `## Reference baseline: moonshine — NOT MEASURED` paragraph
explaining what was tried and what broke. No row is added to the main
table in that case.

## What this harness does (and doesn't) measure

- ✅ Offline WER on the 5-min fixture (`run.py`).
- ✅ Streaming WER at 100 ms and 500 ms cadence (`run_streaming.py`).
- ✅ Streaming-vs-offline WER delta — the differentiating measurement
  vs parakeet-mlx.
- ✅ Partial latency P50/P95 from the streaming harness (with caveat:
  CPU/CUDA, not MLX).
- ✅ Model load time + offline RTF (with caveat: MLX/GPU, not directly
  comparable to CPU-only candidates).
- ✅ Peak RSS via `time -l` (same caveat).
- ✅ Determinism (offline transcript byte-equal across runs).
- ❌ Voxtral Realtime or larger Moonshine variants — out of scope per
  #873; that's a separate spike if Moonshine Medium clears the bar.
- ❌ Cross-platform measurement — Apple-Silicon framing by construction
  on the offline path; the streaming path is incidentally portable but
  not measured as such.

## Files

| File | Tracked | Purpose |
|---|---|---|
| `run.py` | yes | Offline harness via `mlx-audio` |
| `run_streaming.py` | yes | Streaming harness via `useful-moonshine` |
| `requirements.txt` | yes | Pinned deps for both backends |
| `README.md` | yes | This file |
| `.venv/` | **gitignored** | Python env |
| `events*.jsonl` | **gitignored** | Per-run JSONL output |
| `transcript*.txt` | **gitignored** | Per-run transcript |
| `peak_rss*.txt`, `metrics*.txt` | **gitignored** | Analysis dump + `time -l` capture |
