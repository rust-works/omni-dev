# Spike: Voxtral Realtime as streaming-native ASR for `voice listen` (#930)

**Branch:** `issue-930-voxtral-realtime-spike`
**Date:** 2026-06-03
**Time-box:** 2–3 days (used ~½ day)
**Result:** **GO.** Voxtral Realtime eliminates *both* Parakeet streaming defects
(prefix-loss and the streaming-vs-batch WER gap) at sub-second latency and
real-time-capable RTF. Recommended integration path: **MLX Python subprocess
first**, candle port as the pure-Rust endgame. Details below.

## Goal

Decide whether [Mistral Voxtral Realtime Mini 4B](https://huggingface.co/mistralai/Voxtral-Mini-4B-Realtime-2602)
(Apache 2.0) is a viable streaming ASR backend for #807 `voice listen` — as a
replacement for or complement to the Parakeet candle streaming path (#898/#901).
The architectural question: **does Voxtral's causal encoder (no future-looking
attention) eliminate the prefix-loss and lookahead-cost problems measured on
Parakeet's local-attention streaming?**

**Answer: yes, decisively** — see Results.

## What was probed

`mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit` (INT4, ~3 GB) via
[`mlx-audio`](https://github.com/Blaizzy/mlx-audio) `0.4.3`'s dedicated
`voxtral_realtime` streaming backend (`VoxtralStreamingSession`:
`feed()`/`step()`/`close()` → text deltas), on Apple Silicon (64 GB), Python
3.12.12. Harness: [`spike-voxtral/`](spike-voxtral/) — `run_voxtral.py` (paced
streaming sweep), `parse_results.py` (WER lifted verbatim from #856's
`analyze.py` for cross-issue comparability), `smoke.py` (feasibility probe).
Voxtral.c build-feasibility in [`spike-voxtral/voxtral_c/NOTES.md`](spike-voxtral/voxtral_c/NOTES.md).

**Fixture:** the committed `tests/fixtures/voice/monologue_5min.wav` (5 min,
16 kHz mono — same fixture as #826/#856/#871/#898).

**WER ground truth:** the **canonical** Project Gutenberg *A Scandal in Bohemia*
Part I text, trimmed to the fixture's spoken span (626 words). Built at
[`spike-voxtral/canonical/scandal_in_bohemia.txt`](spike-voxtral/canonical/scandal_in_bohemia.txt)
— *not* the committed `monologue_5min.expected.txt` (which has ~23 errors vs
canonical, per #898). The fixture is the LibriVox reading trimmed to 300 s from
the 60 s mark, so the audio begins mid-sentence at "...abhorrent **to his cold,
precise**..." and ends at "...his singular introspective **fashion**." Span
anchors were located via a Voxtral batch (offline) transcription of the fixture.

## Results

### 1. Latency vs accuracy curve (acceptance criterion ✅)

Real-time-paced 80 ms feeds over the full 5-min fixture, one run per delay.
`first-Partial` = wall-clock from stream start to first non-empty Partial
(includes the model's intrinsic audio-accumulation lag; honest proxy, see
harness note). RTF = `sum(step() time) / audio_secs` (pacing-independent).

| `transcription_delay_ms` | effective delay (ms) | first-Partial (ms) | end-of-utterance (ms) | RTF | streaming WER |
|---|---|---|---|---|---|
| 80   | 80   | 640  | 393 | 0.50 | **9.94 %** |
| 240  | 240  | 666  | 389 | 0.46 | **2.84 %** |
| 480  | 480  | 907  | 469 | 0.44 | **3.15 %** |
| 1000 | 1040 | 1467 | 713 | 0.53 | **3.15 %** |

**Offline (batch) WER on the same fixture: 3.15 %** (Voxtral non-streaming).

**Reading of the curve:**
- **No streaming-vs-batch gap.** At 240–1000 ms delay, streaming WER (2.84–3.15 %)
  *equals* offline batch WER (3.15 %). Contrast Parakeet: 9.19 % streaming vs
  2.22 % batch — a ~7 pp gap. Voxtral's causal encoder closes it to ~0.
- **Sweet spot 240–480 ms:** offline-grade ~3 % WER at < 1 s first-Partial.
- Only the most aggressive **80 ms** delay degrades (9.94 %) — and even that
  matches Parakeet's *streaming* WER (9.19 %) while delivering first output at
  640 ms instead of 12–18 s.
- **RTF 0.44–0.53** everywhere → ~2× real-time headroom; live-capable.

### 2. Prefix-loss comparison (acceptance criterion ✅) — the headline

**Parakeet (candle, #898/#901):** the first ~13 s of audio produce **no output**
(first 2 `add_audio` calls emit zero tokens); ~27 prefix words dropped on this
fixture. Inherent to cold-cache local-attention streaming.

**Voxtral:** captures the audio from the **first chunk** — zero prefix loss at
every delay. First 30 words (all configs ≈ identical):

> **Canonical:** *to his cold, precise but admirably balanced mind. He was, I take it, the most perfect reasoning and observing machine that the world has seen, but as a lover he*
>
> **Voxtral @ 480 ms:** *to his cold precise but admirably balanced mind he was i take it the most perfect reasoning and observing machine that the world has seen but as a lover he*

Verbatim match (WER differs only in punctuation/casing, which the scorer
strips). **The causal-encoder hypothesis is confirmed: the prefix-loss failure
mode does not exist on Voxtral.**

### 3. Memory peak (acceptance criterion ✅)

**Peak RSS 4.55 GB** (`/usr/bin/time -l` "maximum resident set size") for the
full sweep — dominated by the INT4 weights (~3 GB) + MLX Metal working buffers
+ Python. (The "peak memory footprint = 45 GB" line is a known macOS metering
artifact across the 4 sequential sessions; RSS is the real number.) Model load
from warm cache: **1.1 s** (mmap'd).

Sizing vs the field: Whisper-tiny.en ~150 MB · Parakeet ~2.5 GB · **Voxtral
~4.55 GB**. Voxtral is the heaviest option — the main cost against its accuracy.

### Honest caveats

- **Cold-start stall.** `max_feed_lag` hit 5–9 s on each run: the first inference
  (Metal kernel JIT + decoder prefill) stalls the single-threaded harness loop,
  which then drains (RTF < 0.55). First-Partial is still 640 ms–1.5 s because
  output starts before the backlog clears. In production, `feed` and `step` run
  on separate threads (the mlx-audio API is built for this), so this
  single-thread artifact overstates the real warmup hit — but a one-time
  ~seconds warmup on model first-use is real and should be hidden behind a
  pre-warm.
- The latency proxy is not word-timestamp-aligned (same limitation as #856); the
  cross-delay comparison is honest because every run is paced identically.

## Updated backend comparison

| Backend | Streaming WER | First-Partial | Prefix-loss | RTF | Peak RSS | License |
|---|---|---|---|---|---|---|
| Parakeet (candle, current) | 9.19 % | ~12–18 s | **drops ~27 words** | — | ~2.5 GB | Apache+CC-BY |
| Whisper-tiny.en (candle, in tree) | ~7 % | ~2–3 s | none | RTF>1 (#856) | ~150 MB | MIT |
| **Voxtral Realtime 4B (MLX INT4)** | **2.84–3.15 %** (≥240 ms) | **0.64–0.91 s** | **none** | **0.44–0.53** | **4.55 GB** | Apache 2.0 |

Voxtral wins WER, first-Partial latency, prefix-loss, and RTF simultaneously;
it loses only on memory.

**Architectural simplification:** because Voxtral's *streaming* WER already
equals its *offline* WER, the dual-path design sketched in #898
(cheap-live-stream + accurate-batch-fix-up-on-endpoint) is **unnecessary** with
Voxtral — one model serves both the live preview and the final transcript. That
removes the Parakeet-batch endpoint stage entirely.

## Integration-path recommendation (acceptance criterion ✅)

Ranked by feasibility-within-budget:

1. **MLX Python subprocess — RECOMMENDED for first ship.**
   Spawn an MLX helper (the proven `VoxtralStreamingSession`), feed s16le 16 kHz
   samples over stdio, receive JSON Partial events. Works *today* (this spike is
   essentially the prototype).
   - **Effort: 2–4 person-days** (productionize IPC framing, process lifecycle/
     pre-warm, package the Python+MLX env). Confidence: **high**.
   - **Cost:** a Python+MLX runtime dependency. This breaks ADR-0033's pure-Rust
     binary-distribution framing (it is *not* a C++ violation — no C++ involved —
     but it is a new runtime dep). **Requires an explicit ADR** before adoption.
     macOS/Apple-Silicon only (MLX is Apple-only), so it's a platform-gated
     backend, not the default.

2. **Voxtral.c — viable Python-free alternative (subprocess or FFI).**
   [`antirez/voxtral.c`](https://github.com/antirez/voxtral.c) compiles clean in
   ~2.6 s; **zero C++**; clean `vox_stream_*` C API (1:1 with what we measured) →
   trivial `bindgen` target, plus a `--stdin` subprocess mode. The **BLAS path is
   pure C + Accelerate → ADR-0033-compatible** (like the existing `onig_sys` C
   dep). See [NOTES.md](spike-voxtral/voxtral_c/NOTES.md).
   - **Effort:** subprocess via `--stdin` ~3–5 days; FFI via `bindgen` ~1–2 weeks.
     Confidence: **medium**.
   - **Cost:** wants the **~8.9 GB bf16** model (no INT4 path → larger disk/RSS
     than the MLX 3 GB INT4); the *fast* path needs Objective-C + Metal (new
     build surface); author flags it as not-yet-production-quality; vendoring a
     3rd-party engine adds maintenance + security-review surface.

3. **Candle port — the pure-Rust endgame, not now.**
   The only fully ADR-0033-aligned option (no runtime deps, cross-platform), but
   Voxtral is a 4 B-param Mistral-architecture model (~6× Parakeet's 0.6 B,
   different arch than Conformer): causal audio encoder + 4× downsample adapter +
   Mistral decoder + INT4 quant + weight conversion + numerical-parity work.
   - **Effort: 4–8 person-weeks.** Confidence: **low** on timeline. Out of a
     spike's scope by definition; this is the follow-up feature if the product
     value proves out.

4. **MLX-Swift bridge — not recommended.** Same MLX backend as option 1 but with
   novel Swift-FFI integration and unknown footprint; the Python subprocess
   dominates it (same model, less integration novelty).

## Recommendation: GO

**Voxtral Realtime is a strict streaming-quality upgrade over the shipped
Parakeet path** — it fixes the two defects the #898/#901 investigation proved
were *inherent* to local-attention streaming (prefix-loss; ~7 pp streaming WER
gap), at **~3 % WER, < 1 s first-Partial, RTF ~0.45**, default delay **240–480 ms**.

**Recommended plan (phased):**

1. **Ship the MLX Python subprocess backend** behind a feature flag /
   platform-gate for `voice listen` (#807) on Apple Silicon. File the follow-up
   feature issue; it **must** include an ADR for the Python+MLX runtime
   dependency (ADR-0033 currently assumes pure-Rust distribution — a Python
   subprocess is a new, non-C++ runtime dep that needs explicit coverage). Use
   it to validate product value with real users on real audio.
2. **Once value is proven, scope a candle port** (option 3) as the cross-platform
   pure-Rust endgame — or adopt Voxtral.c (option 2) if a Python-free native
   path is wanted sooner and the 8.9 GB / Objective-C costs are acceptable.
3. **Retire the dual-path plan** from #898 for the Voxtral path: streaming WER ==
   offline WER, so the Parakeet batch fix-up stage is redundant.

**Do not** keep investing in fixing Parakeet's streaming prefix-loss — it is
architectural and Voxtral is the better answer.

### Follow-up issues to file

- **feat(voice): MLX Voxtral Realtime subprocess backend for `voice listen`** (option 1) — includes the ADR for the Python+MLX runtime dep.
- **spike/feat(voice): candle port of Voxtral Realtime 4B** (option 3) — pure-Rust endgame, ~4–8 wk.

## Reproduce

See [`spike-voxtral/README.md`](spike-voxtral/README.md). TL;DR:

```sh
python3.12 -m venv spike-voxtral/.venv
spike-voxtral/.venv/bin/pip install -r spike-voxtral/requirements.txt
/usr/bin/time -l spike-voxtral/.venv/bin/python spike-voxtral/run_voxtral.py \
    --delays 80 240 480 1000 --out spike-voxtral/results 2> spike-voxtral/results/time.log
spike-voxtral/.venv/bin/python spike-voxtral/parse_results.py
```

(Spike-only throwaway prototype; `.work/` is gitignored. No `src/voice/**`
changes, no CLI surface changes — nothing to merge to `main`.)
