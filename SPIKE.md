# Spike: pure-Rust ASR runtime selection (#813)

**Branch:** `issue-813-asr-runtime-spike`
**Date:** 2026-05-13
**Time-box:** 2 days (consumed: ~half a day; stopped at the day-1 early-exit gate)

## Goal

Pick a pure-Rust ASR runtime that can host a Whisper-style transcription
backend behind the [`Transcriber`](src/voice/transcriber.rs) trait from
#801, satisfying the no-C++-dependencies constraint that deferred
`whisper-rs`.

## Baseline (expected transcript of `short_en.wav`)

Reference: [`whisper-cli`](https://github.com/ggerganov/whisper.cpp) (Homebrew
build) with `ggml-tiny.en.bin`, run on
[tests/fixtures/voice/short_en.wav](tests/fixtures/voice/short_en.wav)
(11.7 s, 16 kHz mono 16-bit PCM, CC0).

> Dark wizards cannot keep their tempers. It is a nearly universal flaw of the species and anyone who makes a habit of fighting them soon learns to rely on it.

Saved verbatim at [baseline/short_en.expected.txt](baseline/short_en.expected.txt).

**Key content words for substring-match acceptance** (see
[baseline/content_words.txt](baseline/content_words.txt)):
`wizards`, `tempers`, `universal`, `flaw`, `species`, `fighting`, `rely`.

A candidate passes the accuracy bar if its transcript contains every
word in that list (case-insensitive substring match).

## Candidates evaluated

### `candle` (`openai/whisper-tiny.en`, non-quantized safetensors)

**Prototype:** [spike-candle/src/main.rs](spike-candle/src/main.rs) — single
binary, ~190 lines, slimmed from
[`candle/candle-examples/examples/whisper`](https://github.com/huggingface/candle/blob/main/candle-examples/examples/whisper/main.rs).
English-only, greedy decode (temperature 0), no timestamps, CPU device,
`hound` for WAV decoding.

| Metric | Value | How measured |
|---|---|---|
| Build time (cold) | **1 m 33 s** | `cargo clean && time cargo build --release` |
| Binary size | **6,191,888 B (≈ 5.91 MiB)** | `stat -f '%z' target/release/spike-candle` |
| Binary size delta vs trivial baseline | **5,790,976 B (≈ 5.52 MiB)** | minus a `cargo new --bin` baseline built with the same `[profile.release]` (LTO=thin, codegen-units=1) |
| C++ deps (literal grep) | **PASS** — empty | `cargo tree --target $(rustc -vV \| sed -n 's/host: //p') \| grep -iE 'cmake\|cc-build\|c\+\+'` |
| HF model fetch (one-time, includes 75 MB safetensors + tokenizer + config) | ~18.7 s | wall-clock from first run |
| Model load (mmap + Whisper struct init) | ~80 ms | timed around `VarBuilder::from_mmaped_safetensors` + `Whisper::load` |
| Inference latency (warm, 3 runs avg) | **~1.16 s** on 11.7 s audio (RTF ≈ 0.10) | timed around `pcm_to_mel` + encoder + greedy decode loop |
| Transcript | `Dark wizards cannot keep their tempers. It is a nearly universal flaw of the species and anyone who makes a habit of fighting them soon learns to rely on it.` | program stdout |
| Accuracy | **PASS** — byte-identical to baseline; all 7 content words present | substring match |

**Determinism.** Three consecutive warm runs produced byte-identical
transcripts and inference times of 1.20 s / 1.16 s / 1.15 s. Greedy
decode at temperature 0 with no random sampling.

**Top-level deps actually used** (`cargo tree --depth 1`):

```
spike-candle v0.0.0
├── anyhow v1.0
├── byteorder v1.5
├── candle-core v0.10.2
├── candle-nn v0.10.2
├── candle-transformers v0.10.2
├── hf-hub v0.4 (features: ureq, rustls-tls)
├── hound v3.5
├── serde_json v1
└── tokenizers v0.22 (default-features = false)
```

#### Honest caveat: transitive C dependency despite passing the grep

The issue's literal acceptance check —
`cargo tree | grep -iE 'cmake|cc-build|c\+\+'` — returns **empty**, so the
defined bar is met. But the dep tree is not fully C-free in spirit:

```
$ cargo tree -i onig
onig v6.5.3
└── tokenizers v0.22.2
    ├── candle-core v0.10.2
    │   └── ...
```

`candle-core` 0.10.2 hard-codes `tokenizers = { features = ["onig"] }` in
its own `Cargo.toml`. Cargo unions features across the dep graph, so
even setting `default-features = false` and omitting `onig` from
spike-candle's direct tokenizers dep does not exclude it.
`onig_sys`'s build script compiles the **oniguruma C library** via the
`cc` crate. Likewise, `ring` (transitively via `rustls` via `hf-hub`'s
`rustls-tls` feature) compiles C and assembly via `cc`.

What this means for the production backend in #802:

- Builds **do** require a working C toolchain (`cc`/`clang`/`gcc`).
  This is normally already a hard requirement on any developer machine
  and on the GitHub-hosted runners we already use, so no immediate
  practical blocker — but it is not the absolute "pure-Rust build" the
  issue's framing implies.
- Neither `oniguruma` nor `ring` involves C++, libstdc++/libc++, or
  cmake — the original `whisper-rs` motivation was specifically a C++
  + cmake build (much heavier failure surface). On that narrower
  comparison, candle is materially better than what we deferred from
  #801.
- Mitigations available to #802 if "no compiled deps at all" becomes a
  hard requirement (none of these are needed today):
  1. Use `[patch.crates-io]` to redirect `candle-core` to a fork that
     drops the `onig` feature from its `tokenizers` line. The
     `tokenizers` crate's regex fallback is `fancy-regex` (pure Rust)
     and is what the spike actually used at runtime — `onig` is linked
     but not exercised by the Whisper tokenizer.
  2. Upstream a PR to `huggingface/candle` to make `onig` non-default
     in `candle-core`'s tokenizers spec.
  3. Swap `rustls-tls` for `native-tls` (uses macOS Security / Windows
     SChannel / OpenSSL) — trades one form of native dep for another;
     doesn't actually help portability.
- The mel-filter blob (`spike-candle/src/melfilters.bytes`, 64 KB) is
  copied verbatim from candle's example and is **data**, not a build
  dependency.

#### Trait fit

A `CandleTranscriber` maps onto
[`Transcriber::transcribe(audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>>`](src/voice/transcriber.rs)
as follows:

- **Construction** (`CandleTranscriber::new(model_path)`): load `config.json`,
  `tokenizer.json`, mel filters, and mmap the safetensors weights into
  the struct once (~80 ms). The struct owns `m::model::Whisper`,
  `Tokenizer`, `Config`, and `Vec<f32>` mel filters; all are `Send`,
  satisfying the `Transcriber: Send + Sync` bound directly without an
  internal `Mutex`. Interior mutability for `forward(.., flush=true)` is
  via `&mut self` on the inference call — `&self` on `transcribe` will
  need a `Mutex<m::model::Whisper>` to gate concurrent transcribe calls
  through a single model instance, exactly the "wrap in `Mutex`" pattern
  the trait's docstring anticipates.
- **`transcribe(audio)`**: drain `audio.next_chunk()` until `None`,
  concatenating into a single `Vec<i16>`; convert to `Vec<f32>`
  (normalised by 32768). Compute mel via
  `candle_transformers::models::whisper::audio::pcm_to_mel`. Run the
  segment-wise decode loop (spike's `run_inference`) — segments of
  `N_FRAMES` mel frames ≈ 30 s of audio each.
- **`Final` emission**: one `Final` per decoded segment. `event_id =
  ulid::Ulid::new()`. `start`/`end` derived from
  `(seek_at_segment_entry × HOP_LENGTH) / SAMPLE_RATE` and the same
  computed after `seek += segment_size`. `text` from
  `tokenizer.decode(&segment_tokens, true)`. `confidence` from the
  segment's `avg_logprob` mapped to `[0, 1]` (e.g., `prob.exp().min(1.0)`).
  `words: None` (greedy decode doesn't expose word timings without
  enabling timestamps mode + per-token timestamp parsing — defer that
  to a later issue if needed). `speaker: None`. `revisable: false`.
- **Terminal `Endpoint`**: after the segment loop completes, emit
  `Endpoint { at: total_duration, kind: EndpointKind::StreamEnd }`.
- **Return**: collect events into a `Vec<Result<TranscriptEvent>>`,
  return `Box::new(events.into_iter())`. The blanket
  `impl<T: Iterator<Item = Result<TranscriptEvent>> + Send> EventStream for T`
  in [`src/voice/transcriber.rs:56-58`](src/voice/transcriber.rs#L56-L58) covers
  this — same shape `MockTranscriber` already uses.
- **`MockTranscriber` analogue**: `CandleTranscriber` and
  [`MockTranscriber`](src/voice/backends/mock.rs) are exactly the same
  shape from the trait's point of view — both ignore the per-chunk
  cadence of the input (drain everything before inference), produce a
  deterministic stream of `Final` events, and emit a terminal
  `StreamEnd` `Endpoint`. The unit-test pattern in
  [tests/voice_transcribe_test.rs](tests/voice_transcribe_test.rs)
  applies unchanged: pass a `VecAudioInput::from_wav_path(short_en.wav,
  chunk)`, collect events, snapshot the JSONL.

## Decision

**Chosen: `candle` (specifically `candle-core` + `candle-nn` +
`candle-transformers` v0.10.x with `openai/whisper-tiny.en` weights).**

Reasoning, in priority order:

1. **It works on day one.** The spike's early-exit rule was "if candle
   clears all bars on day 1, that's a complete spike." Every bar
   cleared: literal C++-freeness grep, accuracy on the committed
   fixture (byte-identical to whisper.cpp baseline), inference latency
   well under real-time (~1.16 s on 11.7 s audio), binary size delta
   reasonable (~5.5 MiB) for a self-contained Whisper backend.
2. **First-party example, minimal redesign.** The slimmed prototype is
   ~190 lines cribbed from candle's own example. The decode loop,
   mel-spectrogram preprocessing, tokenizer wiring, and weights loader
   are all upstream-maintained. We pay zero "wrap an opaque inference
   API" tax — the API is already shaped for the loop we want.
3. **Trait fit is clean and `MockTranscriber`-shaped.** No redesign of
   `Transcriber`, `AudioInput`, or `TranscriptEvent` needed. The
   `Mutex<Whisper>` for thread-safety is the same pattern the trait's
   docstring already anticipates.
4. **Comparison candidates would have been more work, not less.**
   `tract-onnx` ships no Whisper helper — we'd implement
   mel-spectrogram preprocessing and the autoregressive decoder loop in
   userland against raw encoder/decoder ONNX graphs. That's strictly
   more code than candle for the same outcome. The plan said to skip
   it if candle worked; candle worked.
5. **Tract / rten / burn deliberately not measured.** The day-1
   early-exit gate is what justifies stopping; this is documented in
   the spike's plan and in the issue. If the production backend in
   #802 hits a real problem with candle, this spike can be reopened to
   measure the alternatives — but nothing about the current numbers
   suggests that's likely.

The honest C-dep caveat (`onig_sys`, `ring`) does **not** change the
decision: the literal acceptance check passes, the original `whisper-rs`
blocker (C++ + cmake) is genuinely gone, and the practical impact is
limited to "needs a C compiler on the build host", which we already
need.

## What this unblocks

- **#802 (deferred half).** Implementation strategy: a new
  `CandleTranscriber` under `src/voice/backends/candle.rs` (or
  `whisper_candle.rs`); a `"whisper-candle"` arm in
  [`src/voice/factory.rs`](src/voice/factory.rs) that resolves
  `opts.model` to a local `.safetensors` (no HF Hub at runtime — the
  download is a one-time `voice install` or manual step). The
  prototype's `run_inference` becomes the body of the trait method;
  swap `println!` for `TranscriptEvent::Final { ... }` and append a
  `TranscriptEvent::Endpoint`. CLI `--backend whisper-candle --model
  ~/.omni-dev/voice/models/whisper-tiny.en/model.safetensors`.

- **#805 (speaker embedding).** Candle hosts arbitrary safetensors
  models via `VarBuilder::from_mmaped_safetensors`, so the runtime is
  not the constraint. Open question for that issue: which
  speaker-embedding model is going to be used (pyannote vs ECAPA-TDNN
  vs WeSpeaker), and does candle ship a ready-made implementation? The
  Whisper-shaped pattern from this spike applies regardless — only the
  model-specific layer wiring changes. Verify the chosen embedding
  model has a candle reference impl (or is simple enough to port) as a
  prerequisite when #805 starts; if not, the spike scope expands at
  that point, not now.

- **#806 (streaming ASR).** The candle decode loop iterates 30-second
  segments — that's the natural granularity for batch. True
  streaming-with-partials needs a different decoding strategy
  (overlapping windows + partial emit before EOT) and possibly a
  streaming-capable model variant (Whisper-large-v3-turbo decodes
  faster; distil-whisper variants are smaller). #806 should use the
  batch behaviour measured here as its baseline and benchmark
  streaming-specific approaches against it. No change of runtime
  needed.

## What this spike intentionally did not do

- Did **not** implement `Transcriber` against candle. That's #802.
- Did **not** evaluate streaming. That's #806.
- Did **not** evaluate alternative model sizes / quantizations beyond
  `tiny.en` non-quantized — #802's follow-up should profile
  `base.en` and quantized variants for the production default.
- Did **not** patch `candle-core` to drop the `onig` feature. If the
  no-compiled-deps invariant becomes a hard requirement, that's a
  separate, small piece of work documented in the caveat above.
- Did **not** promote this document to `docs/adrs/adr-0033.md`.
  Promotion is a separate small PR per the issue: "The decision doc
  may be promoted to a real ADR via a separate small PR after the
  spike closes."
- Did **not** prototype `tract-onnx`, `rten`, or `burn`. The issue's
  day-1 stop rule is explicit:

  > If candle's Whisper example works on the fixture and clears the
  > C++-freeness check on day 1, that's a complete spike — comparison
  > only happens if the first candidate fails.

  Per-candidate skip rationale, for the record:

  - **`tract-onnx`** (the natural fallback if candle ever has a real
    problem) ships no Whisper helper. A prototype would hand-roll
    mel-spectrogram preprocessing and the autoregressive decoder loop
    against raw encoder/decoder ONNX graphs, plus the same HF
    tokenizer (same `onig` transitive). Estimated cost: ~4–8 hours.
    Accuracy would be identical (same Whisper weights), latency and
    binary size would be different by some margin but not in a way
    that's load-bearing for the choice. Re-open this spike to
    measure it if a candle blocker surfaces.
  - **`rten`** — same general shape as tract-onnx (no Whisper helper),
    smaller community, less battle-tested op coverage for Whisper-shaped
    graphs. The issue itself says "worth a brief look only if `candle`
    and `tract` both have show-stoppers."
  - **`burn`** — the issue says "skip unless candidates 1–3 all fail.
    Less mature for inference workloads." Burn is also not
    ONNX-first; a Whisper prototype would either need a candle-style
    upstream reference impl (there isn't one as polished) or weight
    importing + layer porting, well beyond the spike's time-box.

## Reproduction

```bash
# From the worktree root:

# 1. Establish ground truth (one-time)
brew install whisper-cpp
curl -sL -o models/ggml-tiny.en.bin \
    https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin
whisper-cli -m models/ggml-tiny.en.bin -np -nt tests/fixtures/voice/short_en.wav

# 2. Build and run the candle prototype
cd spike-candle
cargo build --release
./target/release/spike-candle ../tests/fixtures/voice/short_en.wav

# 3. C++-freeness check
cargo tree --target $(rustc -vV | sed -n 's/host: //p') \
    | grep -iE 'cmake|cc-build|c\+\+'   # expect: empty output
```
