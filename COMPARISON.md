# Cross-runtime ASR comparison (sibling artefact to SPIKE.md)

Side-by-side measurements of the three ASR paths in play, all on the **same committed 5-min fixture** ([`tests/fixtures/voice/monologue_5min.wav`](../../tests/fixtures/voice/monologue_5min.wav), LibriVox Sherlock Holmes), against the same hand-corrected [`monologue_5min.expected.txt`](../../tests/fixtures/voice/monologue_5min.expected.txt). All numbers are from prior committed spike work in sibling `.work/` worktrees — no measurements were re-run for this table.

## Three-way comparison

| Metric | parakeet-mlx (fork) | sherpa-onnx-streaming-zipformer-en-2023-06-26 | candle + whisper-tiny.en |
|---|---|---|---|
| **WER** | **3.65 %** (utterance-level, see footnote *a*) | **6.81 %** | **7.13 %** |
| Partial latency P95 — 100 ms chunks (#826 cadence) | **n/a (RTF 2.835, partials queue)** — fork's `32b8034` makes the API *work* at 100 ms, but per-chunk inference is too slow to keep up with realtime on MLX/GPU | n/a in this cell (sherpa was measured at its native ≈450 ms cadence) | n/a (RTF > 1, partials queue indefinitely) |
| Partial latency P95 — ≈ 500 ms chunks | **369 ms** (RTF 1.002, MLX/GPU; informational only — apples-to-apples cadence with sherpa, but different hardware path) | **29 ms** (≈ 450 ms native cadence) | n/a (RTF > 1 at all measured cadences) |
| Time-to-final (silence-driven) | **N/A** (parakeet-mlx surfaces sentence finalisation post-hoc via token-timing segmentation, not via a real-time silence-onset signal) | 1170 ms mean (n=2 of 16) | 1028 ms mean / 2174 ms max |
| Model load | **1985 ms** | 1300 ms | 224 ms (mmap'd safetensors) |
| Model params / on-disk size | 600 M / 2.47 GB | ~70 M / ~260 MB f32 (70 MB int8) | ~39 M / ~40 MB |
| Inference RTF (wall/audio) | **0.016** (MLX/GPU + unified memory; *not* directly comparable to candidates' single-thread CPU numbers — informational only per #856) | ~0.07 (single-thread CPU) | 1.3 paced / 1.37–1.73 no-pacing (single-thread CPU) |
| Peak RSS | **724 MB** (MLX/GPU + unified memory; informational only — not comparable to CPU-only candidates per #856) | not measured | 535 MB (CPU paced) |
| Determinism (two-run transcript hash equality) | ✅ | ✅ (bit-equal text) | ✅ (bit-equal text) |
| Platform | Apple-Silicon-only (MLX/Metal) | cross-platform (linux + macOS + windows) | cross-platform |
| C++-free per [ADR-0033](../../docs/adrs/adr-0033.md) | ❌ (MLX is C++ + Metal) | ❌ (sherpa-onnx is C++) | ✅ |
| Streaming-native architecture | ❌ (utterance-level + running-stats patch in fork's `32b8034`) | ✅ (designed for it) | ❌ (LocalAgreement-2 merger needed) |
| Bundled VAD | ❌ | ✅ Silero | ❌ |
| Measurement source | [#856](https://github.com/rust-works/omni-dev/issues/856) (5-min fixture, MLX/arm64); streaming numbers from `run_streaming.py` at 100 ms and 500 ms chunks | [#859](https://github.com/rust-works/omni-dev/issues/859) findings comment (2026-05-19) | [#826](https://github.com/rust-works/omni-dev/issues/826) SPIKE.md |

*a — WER measured via the utterance-level `model.transcribe()` path. The streaming harness (`run_streaming.py`) produces correct underlying transcript content but a dedupe bug inflates the analyse-script's word count; the streaming WER number is therefore not reported, while partial-latency / RTF (which are unaffected by the dedupe issue) are.*

## Implications for #871's recommendation

**The WER gap that motivated #871 narrows when sherpa is in the picture**: 3.65 % (parakeet) vs 6.81 % (sherpa) is **3.16 pp**, not the ~3.5 pp implied by parakeet-vs-whisper-tiny alone. sherpa already beats whisper-tiny on this fixture by 0.32 pp.

**Streaming-latency-wise, sherpa is decisively ahead — now numerically established.** Running [`baseline/parakeet_mlx/run_streaming.py`](../issue-856-parakeet-mlx-baseline/baseline/parakeet_mlx/run_streaming.py) against the same 5-min fixture with the fork's fix in place:

- **At 100 ms chunks (#826's target cadence):** parakeet-mlx falls to **RTF 2.835** — partials queue at hundreds of seconds behind realtime, indistinguishable from candle's failure mode on this fixture. The 100 ms cadence is unviable for parakeet-mlx on this hardware, *even with* the streaming bug fixed.
- **At 500 ms chunks (closest cadence to sherpa's native ≈450 ms):** parakeet-mlx hits **RTF 1.002** (barely sustains realtime) with **partial latency P95 = 369 ms**. That's **~12× higher** than sherpa's 29 ms at the same cadence, on a *hardware-advantaged* path (MLX/GPU + unified memory vs sherpa's CPU). On candle/CPU — where the candle port would land — parakeet's per-chunk inference cost will almost certainly fail any realistic streaming budget.

This was originally read as evidence that the CPU-RTF gate would be hard to clear: a 15×-bigger model that maxes out MLX/GPU at 500 ms cadence might have nowhere to go on single-thread CPU. **A direct measurement contradicts that read** — see [SPIKE.md § CPU RTF on the candle path (measured)](SPIKE.md#cpu-rtf-on-the-candle-path-measured). The synthetic 24-block encoder bench on Apple M1 Max lands at **RTF 0.27 single-thread / 0.07 multi-thread** on 5-min audio. The MLX-on-GPU streaming numbers above don't generalise to candle-on-CPU because MLX/GPU's bottleneck on this workload is per-chunk dispatch overhead (which scales with chunk-rate), while candle-on-CPU's bottleneck is gemm throughput on long sequences (which favours larger T). Different bottlenecks, different scaling.

The Parakeet candle port recommended by SPIKE.md justifies the ~2.5-week engineering investment if at least one of the following holds — **the previously load-bearing condition 0 (CPU RTF) is now satisfied**:

0. ~~**CPU RTF on the ported encoder is acceptable**~~ ✅ **MEASURED — passes with 5.5× margin.** RTF 0.27 single-thread / 0.07 multi-thread on M1 Max via [SPIKE.md § CPU RTF on the candle path (measured)](SPIKE.md#cpu-rtf-on-the-candle-path-measured). The day-1 gate in the feature task changes from "abort if > 1.5" to "validate the M1 Max numbers reproduce on the production target's lowest-spec CPU".
1. **C++-freeness is non-negotiable** for ASR (current state of ADR-0033) — sherpa-rs is disqualified by construction, and the port closes the WER gap *while preserving the gate*.
2. **3.16 pp WER on long-form audio materially affects the product** — likely true for long dictation / transcription; likely false for commit-message generation (where errors at this rate are mostly punctuation/casing).
3. **Parakeet's larger model genuinely generalises better** to omni-dev's domain mix (technical jargon, names) than zipformer's. **Not measured.** Would need a domain-representative fixture to test.

If conditions 1–3 fail, **sherpa-rs is the cheaper path**: existing measurements, cross-platform, ~zero engineering versus 2.5 weeks for the candle port — and it still beats whisper-tiny.

## What's deliberately NOT compared

- **vs parakeet-tdt-1.1b** or other larger Parakeet variants — out of scope; the model under test is `mlx-community/parakeet-tdt-0.6b-v2`.
- **vs larger Zipformer variants** (e.g. `sherpa-onnx-streaming-zipformer-large-en`) — would close the WER gap further but wasn't part of #859's scope.
- **vs Deepgram / cloud APIs** — different threat model (network dep, paid).
- **vs whisper-base.en / whisper-small.en** — see #856's open follow-up; if `whisper-base.en` closes the gap at < 5 % WER, that's another path that bypasses both #871 and sherpa-rs.
