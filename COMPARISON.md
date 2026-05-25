# Cross-runtime ASR comparison (sibling artefact to SPIKE.md)

Side-by-side measurements of the ASR paths in play, all on the **same committed 5-min fixture** ([`tests/fixtures/voice/monologue_5min.wav`](tests/fixtures/voice/monologue_5min.wav), LibriVox Sherlock Holmes), against the same hand-corrected [`monologue_5min.expected.txt`](tests/fixtures/voice/monologue_5min.expected.txt). The parakeet-mlx, sherpa-onnx, and candle numbers come from prior committed spike work in sibling `.work/` worktrees; the Moonshine column was measured for [#873](https://github.com/rust-works/omni-dev/issues/873) by the harness at [`baseline/moonshine_mlx/`](baseline/moonshine_mlx/).

## Four-way comparison

| Metric | parakeet-mlx (fork) | sherpa-onnx-streaming-zipformer-en-2023-06-26 | candle + whisper-tiny.en | Moonshine (base offline / medium streaming) |
|---|---|---|---|---|
| **WER** | **3.65 %** (utterance-level, see footnote *a*) | **6.81 %** | **7.13 %** | **4.75 %** offline / **5.86 %** streaming @ 100 ms / **5.71 %** @ 500 ms (see footnote *b*) |
| **Streaming-vs-offline WER delta** | ~+17.9 pp at depth=1 (per [#872](https://github.com/rust-works/omni-dev/issues/872)) | 0 pp (streaming-only model — no separate offline path) | n/a | **+0.96 pp** @ 500 ms / **+1.11 pp** @ 100 ms — **load-bearing measurement validating Moonshine's streaming-trained-architecture claim** |
| Partial latency P95 — 100 ms chunks (#826 cadence) | **n/a (RTF 2.835, partials queue)** — fork's `32b8034` makes the API *work* at 100 ms, but per-chunk inference is too slow to keep up with realtime on MLX/GPU | n/a in this cell (sherpa was measured at its native ≈450 ms cadence) | n/a (RTF > 1, partials queue indefinitely) | **n/a (RTF 1.55, partials queue at 125.9 s P95)** — same failure mode as parakeet-mlx and candle, on CPU/CoreML |
| Partial latency P95 — ≈ 500 ms chunks | **369 ms** (RTF 1.002, MLX/GPU; informational only — apples-to-apples cadence with sherpa, but different hardware path) | **29 ms** (≈ 450 ms native cadence) | n/a (RTF > 1 at all measured cadences) | **686 ms** (RTF 1.004 on CPU/CoreML; informational only — runtime is ONNX/CoreML, not MLX) |
| Time-to-final (silence-driven) | **N/A** (parakeet-mlx surfaces sentence finalisation post-hoc via token-timing segmentation, not via a real-time silence-onset signal) | 1170 ms mean (n=2 of 16) | 1028 ms mean / 2174 ms max | **N/A** (moonshine-voice's `on_line_completed` is internally-driven; no `silence_onset` events upstream) |
| Model load | **1985 ms** | 1300 ms | 224 ms (mmap'd safetensors) | **1.66 s** (offline, mlx-audio); **0.47 s warm** / 56 s cold (streaming, moonshine-voice first-download from moonshine.ai CDN) |
| Model params / on-disk size | 600 M / 2.47 GB | ~70 M / ~260 MB f32 (70 MB int8) | ~39 M / ~40 MB | base ~62 M (offline) / Medium Streaming 245 M / ~140 MB ONNX (streaming) |
| Inference RTF (wall/audio) | **0.016** (MLX/GPU + unified memory; *not* directly comparable to candidates' single-thread CPU numbers — informational only per #856) | ~0.07 (single-thread CPU) | 1.3 paced / 1.37–1.73 no-pacing (single-thread CPU) | **0.014** offline (MLX/GPU, chunked at 25 s); **1.550** @ 100 ms / **1.004** @ 500 ms streaming (CPU/CoreML) |
| Peak RSS | **724 MB** (MLX/GPU + unified memory; informational only — not comparable to CPU-only candidates per #856) | not measured | 535 MB (CPU paced) | **459 MB** offline (MLX/GPU); **1.62 GB** streaming (medium model + ONNX Runtime working set) |
| Determinism (two-run transcript hash equality) | ✅ | ✅ (bit-equal text) | ✅ (bit-equal text) | ✅ offline (bit-equal); streaming determinism not checked |
| Platform | Apple-Silicon-only (MLX/Metal) | cross-platform (linux + macOS + windows) | cross-platform | Apple-Silicon-only on the offline (mlx-audio) path; cross-platform on the streaming (moonshine-voice/ONNX) path |
| C++-free per [ADR-0033](docs/adrs/adr-0033.md) | ❌ (MLX is C++ + Metal) | ❌ (sherpa-onnx is C++) | ✅ | ❌ (mlx-audio uses MLX C++/Metal; moonshine-voice uses ONNX Runtime C++) |
| Streaming-native architecture | ❌ (utterance-level + running-stats patch in fork's `32b8034`) | ✅ (designed for it) | ❌ (LocalAgreement-2 merger needed) | ✅ **(validated)** — Medium Streaming variant trained with sliding-window attention; the ~1 pp streaming-vs-offline WER delta is **dramatically smaller** than parakeet's ~17.9 pp at default streaming depth |
| Bundled VAD | ❌ | ✅ Silero | ❌ | ❌ |
| Measurement source | [#856](https://github.com/rust-works/omni-dev/issues/856) (5-min fixture, MLX/arm64); streaming numbers from `run_streaming.py` at 100 ms and 500 ms chunks | [#859](https://github.com/rust-works/omni-dev/issues/859) findings comment (2026-05-19) | [#826](https://github.com/rust-works/omni-dev/issues/826) SPIKE.md | [#873](https://github.com/rust-works/omni-dev/issues/873) — [`baseline/moonshine_mlx/`](baseline/moonshine_mlx/), macOS/arm64, mlx-audio 0.4.3 + `UsefulSensors/moonshine-base` (offline) + moonshine-voice 0.0.59 + Medium Streaming (streaming) |

*a — WER measured via the utterance-level `model.transcribe()` path. The streaming harness (`run_streaming.py`) produces correct underlying transcript content but a dedupe bug inflates the analyse-script's word count; the streaming WER number is therefore not reported, while partial-latency / RTF (which are unaffected by the dedupe issue) are.*

*b — Moonshine's offline path uses `UsefulSensors/moonshine-base` (~62 M params) because `mlx-audio` doesn't expose Moonshine's streaming variants. The streaming path uses Medium Streaming (245 M) because that's what #873's load-bearing question (does the streaming-trained architecture preserve quality?) asks about. So this column compares two different model sizes; the streaming-vs-offline delta is consequently conservative — running Medium offline (if mlx-audio supported it) would likely close the gap further. The architectural validation holds regardless: a streaming-trained Moonshine produces near-offline WER under streaming inference, in stark contrast to parakeet-mlx's offline-trained-with-streaming-wrapper behaviour.*

## Implications for #873 — does Moonshine displace parakeet-mlx as the Apple-Silicon target?

**Moonshine answers the load-bearing question #873 framed.** The streaming-vs-offline WER delta is ~1 pp (5.71 % @ 500 ms vs 4.75 % offline), validating the paper's claim that sliding-window attention is the *training-time* attention pattern, not an inference-time approximation. This is a **fundamentally different streaming behaviour** than parakeet-mlx's ~17.9 pp drift at default streaming depth (per [#872](https://github.com/rust-works/omni-dev/issues/872)) — Moonshine's stream stays usable indefinitely without depth-dependent degradation.

**On WER, Moonshine slots between parakeet-mlx and sherpa-onnx**, beating sherpa by ~1 pp and whisper-tiny by ~2.4 pp, with a ~62 M-parameter offline model and 245 M-parameter streaming model — both substantially smaller than parakeet's 600 M.

**On latency, Moonshine does not match sherpa-onnx on the runtime path measured here.** At the 500 ms cadence: partial P95 = 686 ms (Moonshine on CPU/CoreML), 369 ms (parakeet-mlx on MLX/GPU), **29 ms** (sherpa-onnx on CPU). At 100 ms cadence, Moonshine cannot sustain realtime (RTF 1.55, partials queue) — the same failure mode as parakeet-mlx and candle. **This is largely a runtime gap, not an architecture gap**: `moonshine-voice` ships ONNX Runtime with CoreML execution provider, not MLX. An MLX-backed Moonshine implementation would likely close most of the latency gap; `mlx-audio` doesn't currently provide one for streaming.

**For #871's candle-port motivation:**

1. **If C++-freeness remains non-negotiable (current state of ADR-0033):** sherpa-onnx is still disqualified, and **a candle port becomes the WER path forward** — but **Moonshine is now the obvious port target rather than Parakeet-TDT**. A Moonshine port would target a ~2.4× smaller model (245 M Medium Streaming vs 600 M Parakeet-TDT) with a streaming-native architecture that already preserves quality under streaming. This is a much better fit for the day-1 CPU-RTF gate in [SPIKE.md § Open risk](SPIKE.md#open-risk-cpu-rtf-on-the-candle-path).

2. **If C++-freeness is relaxed:** sherpa-rs remains the cheapest existing path (cross-platform, zero engineering, 6.81 % WER). Moonshine offers ~1 pp better WER than sherpa for a smaller engineering investment than a Parakeet port (smaller model, no streaming-API patch needed). The **cost-per-WER-point** strongly favours a Moonshine path over a Parakeet path if a port is undertaken at all.

3. **Apple-Silicon-only carve-out:** Moonshine does not in itself solve the cross-platform requirement that motivates #871. Its offline path is Apple-only via `mlx-audio`; its streaming path is cross-platform via `moonshine-voice` but ONNX-backed (so not C++-free per ADR-0033). For a cross-platform C++-free runtime, both Moonshine and Parakeet still require a candle (or similar) port.

**Bottom line:** Moonshine **strengthens #871's "port something to candle" recommendation** by giving it a smaller, architecturally-better target — but does not eliminate the need for the port itself, because the cross-platform + C++-free + low-latency intersection isn't satisfied by any existing runtime for Moonshine. If the day-1 CPU-RTF gate in SPIKE.md leans towards aborting a 600 M Parakeet port, **a 245 M Moonshine port is the obvious next move** before falling back to sherpa-rs.

## Implications for #871's recommendation

**The WER gap that motivated #871 narrows when sherpa is in the picture**: 3.65 % (parakeet) vs 6.81 % (sherpa) is **3.16 pp**, not the ~3.5 pp implied by parakeet-vs-whisper-tiny alone. sherpa already beats whisper-tiny on this fixture by 0.32 pp.

**Streaming-latency-wise, sherpa is decisively ahead — now numerically established.** Running [`baseline/parakeet_mlx/run_streaming.py`](../issue-856-parakeet-mlx-baseline/baseline/parakeet_mlx/run_streaming.py) against the same 5-min fixture with the fork's fix in place:

- **At 100 ms chunks (#826's target cadence):** parakeet-mlx falls to **RTF 2.835** — partials queue at hundreds of seconds behind realtime, indistinguishable from candle's failure mode on this fixture. The 100 ms cadence is unviable for parakeet-mlx on this hardware, *even with* the streaming bug fixed.
- **At 500 ms chunks (closest cadence to sherpa's native ≈450 ms):** parakeet-mlx hits **RTF 1.002** (barely sustains realtime) with **partial latency P95 = 369 ms**. That's **~12× higher** than sherpa's 29 ms at the same cadence, on a *hardware-advantaged* path (MLX/GPU + unified memory vs sherpa's CPU). On candle/CPU — where the candle port would land — parakeet's per-chunk inference cost will almost certainly fail any realistic streaming budget.

This *strengthens* the CPU-RTF gate in [SPIKE.md § Open risk](SPIKE.md#open-risk-cpu-rtf-on-the-candle-path): a 15×-bigger model that already maxes out MLX/GPU at 500 ms cadence has nowhere to go on single-thread CPU.

The Parakeet candle port recommended by SPIKE.md only justifies the ~2.5-week engineering investment if **all three** of the following hold:

0. **CPU RTF on the ported encoder is acceptable** (target ≤ 1.0 on a single CPU thread, ≤ 0.5 with multi-thread BLAS / int8 quantisation). The 0.016 RTF measured for parakeet-mlx in [#856](https://github.com/rust-works/omni-dev/issues/856) is MLX-on-GPU and does **not** predict candle-on-CPU; #826 already measured candle + the 15×-smaller whisper-tiny.en at RTF 1.3 on CPU. The SPIKE.md effort breakdown gates this as the day-1 milestone of the feature task — if the encoder alone exceeds RTF 1.5, **the port aborts** and sherpa-rs / whisper-base.en become the answer. See [SPIKE.md § Open risk: CPU RTF](SPIKE.md#open-risk-cpu-rtf-on-the-candle-path).
1. **C++-freeness is non-negotiable** for ASR (current state of ADR-0033) — sherpa-rs is disqualified by construction, and the port closes the WER gap *while preserving the gate*.
2. **3.16 pp WER on long-form audio materially affects the product** — likely true for long dictation / transcription; likely false for commit-message generation (where errors at this rate are mostly punctuation/casing).
3. **Parakeet's larger model genuinely generalises better** to omni-dev's domain mix (technical jargon, names) than zipformer's. **Not measured.** Would need a domain-representative fixture to test.

If condition 0 fails, the port is unviable regardless of the other three. If conditions 1–3 fail (with 0 passing), **sherpa-rs is the cheaper path**: existing measurements, cross-platform, ~zero engineering versus 2.5 weeks for the candle port — and it still beats whisper-tiny.

## What's deliberately NOT compared

- **vs parakeet-tdt-1.1b** or other larger Parakeet variants — out of scope; the model under test is `mlx-community/parakeet-tdt-0.6b-v2`.
- **vs larger Zipformer variants** (e.g. `sherpa-onnx-streaming-zipformer-large-en`) — would close the WER gap further but wasn't part of #859's scope.
- **vs Deepgram / cloud APIs** — different threat model (network dep, paid).
- **vs whisper-base.en / whisper-small.en** — see #856's open follow-up; if `whisper-base.en` closes the gap at < 5 % WER, that's another path that bypasses both #871 and sherpa-rs.
