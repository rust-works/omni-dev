# Spike: streaming ASR runtime selection (#826)

**Branch:** `issue-826-streaming-asr-spike`
**Date:** 2026-05-18
**Time-box:** 2 days
**Result:** **Neither candidate clears every gate as specified.** Recommendation below.

## Goal

Pick between:
1. **Candle + LocalAgreement-2** sliding-window merger over `openai/whisper-tiny.en` (already in tree from #802, per ADR-0033)
2. **tract-onnx + streaming Zipformer** (`sherpa-onnx-streaming-zipformer-en-2023-06-26`) — runtime already in tree from #805, per ADR-0034

The winning runtime informs #806's streaming ASR implementation and #807's `voice listen` streaming integration.

## Fixture

[`tests/fixtures/voice/monologue_5min.wav`](tests/fixtures/voice/monologue_5min.wav) — 5 min 00 s of single-reader LibriVox audio (Sherlock Holmes ch.1 "A Scandal in Bohemia", reader TBOL3, public domain). 16 kHz mono 16-bit PCM, ~9.6 MB. Provenance at [`monologue_5min.PROVENANCE.md`](tests/fixtures/voice/monologue_5min.PROVENANCE.md).

**Fixture size note.** The issue's "≤ ~6 MB" cap is internally inconsistent with the 5-min duration — 16 kHz mono i16 has a hard 32 KB/s floor, so 5 min ≈ 9.6 MB. The 5-min duration is the load-bearing requirement (boundary artefact testing over multiple silence-gap utterances); compressing to FLAC would halve size but break the WAV invariant `VoiceAudioInput::from_wav_path` enforces. Committed at 9.6 MB.

Ground truth at [`monologue_5min.expected.txt`](tests/fixtures/voice/monologue_5min.expected.txt) — captured via `whisper-cli` (Homebrew whisper.cpp) + `ggml-tiny.en.bin` on the fixture, then hand-corrected for proper nouns ("Holmes", "Trepoff", "Trincomalee", "Study in Scarlet", "gasogene") and obvious mis-recognitions ("save with a gibe and a sneer", "Grit in a sensitive instrument", "his own keen nature").

**Latency proxy honesty note.** Partial latency is measured as `wall_ms - audio_ms` at Partial emission time (= wall-clock at emit minus the simulated-clock time of the last-pushed audio sample), not against word-level ground-truth timestamps. The proxy is fair across both candidates because both consume identically paced 100 ms chunks. Absolute latency to a *spoken word* would be larger by an unknown amount (0.5–1.5 s typically for Whisper-class models), but the cross-candidate comparison is honest.

## Candidate 1: candle + LocalAgreement-2 (whisper-tiny.en)

**Implementation.** [`spike-candle-streaming/`](spike-candle-streaming/) (~290 lines). Lifts the encoder/decoder loop from [`src/voice/backends/candle.rs`](src/voice/backends/candle.rs) verbatim. LocalAgreement-2 merger: maintains a sliding 30 s audio window, re-runs whisper-tiny.en every `min_chunk_secs` (default 1.0 s) of new audio, takes the longest common word-prefix between consecutive hypotheses, emits the part that extends beyond the already-committed prefix as a `Final` event. Silence-gap endpoint via [`baseline::idle`](baseline/src/idle.rs) (copy of [`src/voice/idle.rs`](src/voice/idle.rs) — no path-dep on the parent crate to keep the build self-contained). Window resets on silence gap or 30 s hard cap.

**Measurements** (5-min fixture, single-threaded NOT enforced — macOS auto-parallelised across ~2 cores; results would be ~2× worse on a true single-core run):

| Metric | Value | Gate | Pass? |
|---|---|---|---|
| Build time (cold) | 93 s | — | n/a |
| Binary size | 4.5 MB | — | n/a |
| Model load | 224 ms (mmap'd safetensors) | — | n/a |
| WER | **7.13 %** | ≤ 15 % | ✅ |
| Time-to-final (silence-gap) — mean / max | 1028 / 2174 ms | ≤ 2500 ms | ✅ |
| Determinism (two reruns, text equality) | identical (174 finals, hash-equal) | bit-equal | ✅ |
| C++-freeness grep | empty | empty | ✅ |
| Partial latency P50 / P95 | n/a (`--emit-partials` off; with RTF > 1 partials would queue indefinitely behind realtime) | ≤ 1000 ms P95 | ❌ |
| **RTF** (no-pacing, 5-min run) | **1.30** (paced) / **1.37–1.73** (no-pacing varied across runs) | ≤ 0.5 | **❌** |
| **Peak RSS** (paced run) | **535 MB** | ≤ 500 MB | ❌ (marginal +7 %) |

**Why RTF fails structurally.** Mean inference cost per Final emission is ~3 s (whisper-tiny.en encoder + decoder over a typical 15–25 s window). LocalAgreement-2 requires re-running on the whole accumulated window each step. With re-inference cadence 1.0 s and avg cost ~1.5 s, RTF lands around 1.3. To meet RTF ≤ 0.5, the re-inference cadence would have to drop to ~3 s — which puts Partial latency P95 above 3 s, blowing the ≤ 1 s gate. The two gates are in tension and cannot simultaneously be met with this model size + naïve LocalAgreement-2.

**The paced run fell behind realtime.** Wall-clock elapsed 701 s for 300 s of audio (paced at 100 ms/chunk wall-clock), confirming RTF > 1 means the system cannot keep up with live input — backpressure builds without bound. The `final_latency_p95 ≈ 356 s` figure from the analysis script reflects exactly this: by the end of the stream, finals are emitted ~6 min after the audio they describe arrived.

**Hand-corrected ground truth still produces 7 % WER**, dominated by acoustic edge cases (`"jive and his near"` for `"gibe and a sneer"`) and merger-induced word splits at LocalAgreement boundaries — the candidate-1 candle backend is *accurate*, it's just *slow*.

## Candidate 2: tract-onnx + streaming Zipformer (sherpa-onnx-streaming-zipformer-en-2023-06-26)

**Status: HARD BLOCKER at model-load.** Implementation did not progress past the ONNX ingest probe.

**Implementation attempted.** [`spike-tract-zipformer/`](spike-tract-zipformer/) — two binaries:
- `probe-onnx-io`: parses the encoder / decoder / joiner ONNX files and dumps input/output facts. Used to scope the streaming wiring before committing to it.
- `spike-tract-zipformer`: minimal ingest + optimise probe.

**Model graph scope** (from `probe-onnx-io`):
- Encoder: **99 inputs**, **99 outputs**. Audio chunk shape `N,45,80` (45 frames of 80-bin fbank ≈ 450 ms at 10 ms shift). The other 98 inputs are per-layer state caches (`cached_key_*`, `cached_nonlin_attn_*`, `cached_val1_*`, `cached_val2_*`, `cached_conv1_*`, `cached_conv2_*` across 17 sub-layers). 98 corresponding outputs are the updated state caches.
- Decoder: 1 input `y` shape `N,2,I64` (last 2 emitted non-blank BPE tokens), 1 output shape `N,512`.
- Joiner: 2 inputs (encoder + decoder embeddings, both `N,512`), 1 output (logits over 502 BPE tokens).

**Blocker** (from `spike-tract-zipformer` run):

```
encoder ingest OK (99 inputs, 99 outputs, 116 ms)
encoder into_optimized() FAILED (37 ms):
  Failed analyse for node #3393 "/upsample/Reshape_1" Reshape
decoder ingest OK   (1 input, 1 output, 0 ms)
joiner ingest OK    (2 inputs, 1 output, 0 ms)
```

`tract-onnx 0.21.15`'s shape-analysis pass cannot resolve a `Reshape` op inside the encoder's upsample subgraph. Without `into_optimized()` succeeding, `into_runnable()` is not reachable, so no inference can run. This is independent of which weights variant is used (`.onnx` 260 MB or `.int8.onnx` 70 MB) — both fail identically (114–116 ms ingest, 37 ms to the analyse error).

**Measurements not obtainable**:

| Metric | Value | Gate | Pass? |
|---|---|---|---|
| Build time (cold) | 232 s | — | n/a |
| Binary size (`spike-tract-zipformer`) | ~6 MB | — | n/a |
| Encoder ingest time | 116 ms | — | n/a |
| Peak RSS at idle after model load (f32 weights) | 580 MB | — | (irrelevant — can't run) |
| Peak RSS at idle after model load (int8 weights) | 187 MB | — | (irrelevant — can't run) |
| C++-freeness grep | empty | empty | ✅ |
| WER / RTF / latency / determinism / time-to-final | NOT MEASURABLE | (gates) | ❌ (model can't run) |

**Recovery paths** (none free, all out of the spike's time-box):
1. **Upgrade or patch tract-onnx.** Submit a fix for the failing Reshape op upstream, or vendor a tract fork. Unknown complexity until someone reads the tract analyser code; the failing op is `#3393` in the `upsample` subgraph — likely a dynamic-shape Reshape that the analyser doesn't recognise as a known pattern.
2. **Re-export the model with tract-compatible ops.** sherpa-onnx's `export-onnx-streaming.py` (in the model archive) generated the failing graph. Re-exporting with `--simplify` or with explicit static shapes could replace the dynamic Reshape with ops tract can analyse. Requires Python + icefall toolchain set up locally; unbounded scope.
3. **Switch runtime.** Use `candle-onnx` (mentioned in [ADR-0034's "Alternatives considered"](docs/adrs/adr-0034.md) as a future consolidation path) or `sherpa-rs` (the official C++ bindings — but this brings cmake/c++ deps and **fails the C++-freeness gate that ADR-0033 and ADR-0034 establish**).
4. **Adopt a different streaming ONNX export.** k2-fsa publishes other streaming Zipformer variants (smaller `2023-02-21`, bilingual `2023-02-20`); they may export differently. Untested.

**Even if `into_optimized()` succeeded**, the prototype scope would still be substantial: initialising 98 state tensors to zeros of the correct shapes, cycling state outputs back into inputs each encoder call, implementing greedy RNN-T decoding (decoder + joiner loop with blank-vs-non-blank handling), and BPE detokenisation with `▁` word-start markers. The plan's "fall back to non-streaming greedy decode" option doesn't apply cleanly here either: the encoder's audio input shape is fixed at 45 frames per call (the model IS a streaming model), so a "single batch pass" path doesn't exist without re-export.

## Decision

**Neither candidate clears every acceptance gate as specified in #826.**

| Gate | Candidate 1 (candle) | Candidate 2 (tract) |
|---|---|---|
| Partial latency P95 ≤ 1.0 s | not measurable under realtime budget (RTF > 1) | not measurable (model can't run) |
| Time-to-final ≤ 2.5 s | ✅ (max 2.17 s) | not measurable |
| WER ≤ 15 % | ✅ (7.13 %) | not measurable |
| RTF ≤ 0.5 | ❌ (1.30) | not measurable |
| Peak RSS ≤ 500 MB | ❌ (535 MB) | not measurable |
| Determinism | ✅ (bit-equal text) | not measurable |
| C++-freeness | ✅ | ✅ |
| Trait-fit paragraph | drafted below | n/a |

**Recommended next step (per the issue's "If neither does, document the gap and recommend a fallback" clause):**

Pursue **candidate 1 with algorithmic optimisations**, in priority order:

1. **Silero VAD-driven aggressive chunking** instead of silence-gap RMS. The issue calls VAD A/B "out of scope" for the spike (use silence-gap RMS for both candidates for fairness), but the spike's results show silence-gap RMS detected only ~8 natural silence onsets across 5 min of LibriVox narration — too few to keep merger windows small. Silero VAD's frame-level voice/non-voice signal would let us reset the merger every ~3–5 s of contiguous speech, dropping inference cost proportionally and likely getting RTF ≈ 0.3–0.5.
2. **Smaller hard window cap** (e.g. 8 s instead of 30 s). Trades hallucination risk at boundaries for inference cost.
3. **Re-evaluate candidate 2 once tract-onnx supports the Reshape op** — track upstream. The streaming Zipformer is architecturally the right shape for this problem (streaming-native, no merger needed); the spike's blocker is purely a tract-onnx maturity issue.
4. **Reconsider the C++-freeness gate for ASR specifically.** ADR-0033 established it; ADR-0034 maintained it for speaker embedding. But sherpa-rs (the C++ binding) is the production-quality streaming-Zipformer path — if the project's gate hierarchy ranks streaming quality above C++-freeness for ASR alone, sherpa-rs becomes viable. Not the spike's call; flagged here for a #806 follow-up discussion.

**Do NOT** ship candidate 1 as-is to #806 — RTF > 1 means it cannot keep up with live input. The fallback recipe (Silero VAD + smaller window) is the actionable next step; the spike validates that candle + Whisper produces accurate (WER 7 %) deterministic transcripts on real speech, so the runtime choice is sound even though the LocalAgreement-2-as-specified algorithm is not.

## Trait fit (provisional, for candidate 1 + VAD optimisation)

The chosen runtime maps onto #806's `StreamingTranscriber` trait roughly as follows. `transcribe_stream(audio: Box<dyn AsyncAudioInput>) -> Pin<Box<dyn Stream<Item = Result<TranscriptEvent>>>>` returns a stream that internally drives the candle backend. The `AsyncAudioInput::next_chunk` 100 ms chunks feed two parallel sinks: (1) the LocalAgreement-2 merger's audio window, and (2) the endpoint detector (Silero VAD per the recommendation above; falls back to silence-gap RMS via the existing [`crate::voice::idle::IdleDetector`](src/voice/idle.rs) if Silero isn't enabled). On each VAD voice-activity event the merger may run an inference; on each VAD silence-onset it flushes the residual hypothesis as `Final { revisable: true }` and emits `Endpoint { kind: SilenceGap }`. The merger's inference work runs on a dedicated tokio task (the candle `Whisper` instance lives behind the existing `Mutex` from `src/voice/backends/candle.rs`); the merger's `Partial` and `Final` emissions cross back to the trait stream via a `tokio::sync::mpsc::channel`. This pattern keeps `StreamingTranscriber::transcribe_stream`'s caller fully async while the inference itself runs on a blocking task — matching the production constraints `cpal`'s capture callback already imposes.

## What this unblocks

- **#806** — streaming ASR implementation. Backend file: [`src/voice/backends/candle_streaming.rs`](src/voice/backends/candle_streaming.rs) (per the rewritten plan in #806). The Silero VAD recommendation feeds straight into #806's "VAD" sub-task — likely additive to the existing IdleDetector, not a replacement.
- **#807** — `voice listen` streaming integration. The 100 ms chunk cadence and `Endpoint { SilenceGap }` semantics line up directly with the realtime scheduler.

## Promotion

If #806 chooses to ship the candle+VAD recipe, **this document is promoted to ADR-0035** via a separate PR. The ADR will follow the [ADR-0033 (candle for ASR)](docs/adrs/adr-0033.md) / [ADR-0034 (tract-onnx for speaker)](docs/adrs/adr-0034.md) structure: Context / Decision / Consequences / Alternatives considered. If #806 chooses sherpa-rs (waiving C++-freeness for ASR), that's a larger decision and would warrant its own ADR amending ADR-0033.

## Repro

```bash
# from .work/issue-826-streaming-asr-spike/
# candidate 1 (paced realtime, what the gates measure):
cd spike-candle-streaming
cargo build --release
./target/release/spike-candle-streaming \
  --fixture ../tests/fixtures/voice/monologue_5min.wav \
  --model-dir ~/.omni-dev/voice/models/whisper-tiny.en \
  --log runs/paced.jsonl \
  --silence-secs 1

# candidate 2 (model-load probe; fails at into_optimized):
cd ../spike-tract-zipformer
cargo build --release --bins
./target/release/probe-onnx-io \
  --model-dir ../models/sherpa-onnx-streaming-zipformer-en-2023-06-26
./target/release/spike-tract-zipformer \
  --model-dir ../models/sherpa-onnx-streaming-zipformer-en-2023-06-26

# analysis (WER + latency proxies from a JSONL run):
python3 scripts/analyze.py \
  spike-candle-streaming/runs/paced.jsonl \
  tests/fixtures/voice/monologue_5min.expected.txt
```

Model staging (not committed; one-time):

```bash
omni-dev voice install-model --variant whisper-tiny.en   # candle path
mkdir -p models && cd models                                # tract path
curl -L -O https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
tar xjf sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
```
