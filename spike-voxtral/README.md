# Voxtral Realtime streaming probe (#930)

One-off harness measuring **Mistral Voxtral Realtime Mini 4B** (via the
[`mlx-audio`](https://github.com/Blaizzy/mlx-audio) `voxtral_realtime` backend,
INT4 checkpoint `mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit`) as a
candidate streaming ASR backend for `voice listen` (#807). Throwaway prototype —
**not** a Rust port, **not** production code. Output is `../SPIKE.md`.

## Host requirements

- macOS / Apple Silicon (MLX is Apple-only).
- **Python 3.12** — 3.14 is too new for the MLX wheels (same constraint as #856;
  `python3.12` is on this host as `/opt/homebrew/bin/python3.12`).
- ~3 GB free disk + network for the one-time HuggingFace model fetch.

## Setup

```sh
python3.12 -m venv spike-voxtral/.venv
spike-voxtral/.venv/bin/pip install -r spike-voxtral/requirements.txt
```

## Run

```sh
# (optional) feasibility smoke test — batch + streaming on first 20 s:
spike-voxtral/.venv/bin/python spike-voxtral/smoke.py

# full sweep: paced 80 ms feeds over the 5-min fixture, one run per delay.
# Captures peak RSS for the whole sweep via /usr/bin/time -l.
/usr/bin/time -l spike-voxtral/.venv/bin/python spike-voxtral/run_voxtral.py \
    --delays 80 240 480 1000 --out spike-voxtral/results 2> spike-voxtral/results/time.log

# score: WER + latency curve + prefix-loss comparison (markdown to stdout)
spike-voxtral/.venv/bin/python spike-voxtral/parse_results.py \
    --results spike-voxtral/results \
    --reference spike-voxtral/canonical/scandal_in_bohemia.txt
```

## Files

| File | Role |
|---|---|
| `smoke.py` | Downloads the checkpoint; batch + streaming sanity probe. |
| `run_voxtral.py` | Streaming harness: paced feeds, per-Partial event log, summaries. |
| `parse_results.py` | WER (lifted from #856) + latency curve + prefix-loss. |
| `canonical/scandal_in_bohemia.txt` | WER ground truth — canonical Gutenberg text trimmed to the fixture's spoken span (see file header). |
| `results/` | Raw timings + transcripts (gitignored via `/.work/`). |

## Latency proxy honesty note

`first_partial_wall_ms` is wall-clock from stream start (t0) to the first
non-empty Partial, under real-time-paced 80 ms feeds — it INCLUDES the model's
intrinsic `transcription_delay_ms` audio-accumulation lag, but is not measured
against word-level ground-truth timestamps. RTF is pacing-independent
(`sum(step() time) / audio_secs`). See the docstring in `run_voxtral.py`.
