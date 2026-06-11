# ASR Backends

How `omni-dev voice transcribe` turns 16 kHz mono WAV audio into transcript
events, and how to choose between the available speech-to-text backends.

For the AI (LLM) backends used by commit-message generation, see
[ai-backends.md](ai-backends.md) — this document covers **speech recognition**
only.

## Backend overview

| Backend                    | Selection string           | Kind      | Latency class                 | Platforms | Model              | ADR                            |
|----------------------------|----------------------------|-----------|-------------------------------|-----------|--------------------|--------------------------------|
| Mock                       | `mock` (default)           | canned    | —                             | all       | none               | —                              |
| Whisper batch              | `whisper-candle`           | batch     | full-file (offline)           | all       | `whisper-tiny.en`  | [ADR-0033](adrs/adr-0033.md)   |
| Whisper streaming (LCD)    | `whisper-candle-streaming` | streaming | **bounded ~1.5–3 s lag**      | all       | `whisper-tiny.en`  | [ADR-0040](adrs/adr-0040.md)   |

All backends are pure Rust with no native-toolchain requirement (no
C++/CMake), which is what makes them the cross-platform floor — including
Windows.

## Selecting a backend

Backend choice flows from, in order:

1. `--backend <name>` on the command line,
2. the `OMNI_DEV_VOICE_BACKEND` environment variable,
3. the default: `mock`.

```bash
omni-dev voice transcribe recording.wav --backend whisper-candle-streaming
```

## Installing the model

Both Whisper backends share the same model files:

```bash
omni-dev voice install-model            # stages whisper-tiny.en
```

Files land in `~/.omni-dev/voice/models/whisper-tiny.en/`. Override the
location with `--model <dir>` or `OMNI_DEV_VOICE_WHISPER_MODEL=<dir>`.

## `whisper-candle` (batch)

Decodes the entire input in one pass and emits one `Final` event per ~30 s
segment plus a terminal `Endpoint`. Right choice when the audio already
exists as a file and latency is irrelevant. See
[ADR-0033](adrs/adr-0033.md).

## `whisper-candle-streaming` (cross-platform streaming LCD)

The **latency-tolerant, lowest-common-denominator streaming tier**
([#974](https://github.com/rust-works/omni-dev/issues/974), validated by the
[#969](https://github.com/rust-works/omni-dev/issues/969) spike): VAD-gated
chunking + cadence re-decode + LocalAgreement-2 commit over the same candle
Whisper inference the batch backend uses. Events stream lazily as audio is
consumed: committed text arrives as non-revisable `Final`s, the volatile
hypothesis tail as `Partial`s, utterance boundaries as `Endpoint`s.

### The latency caveat — read this before choosing it

The displayed transcript trails the speaker by **~1.5 s typical, up to ~3 s**.
This is structural, not tunable: candle Whisper pays a fixed ~0.5–0.6 s per
inference (fixed-size encoder, no streaming KV-cache), so sub-second
interactive latency is a **non-goal** for this backend
([ADR-0040](adrs/adr-0040.md) records the root cause and the rejected
work-arounds). The lag is **bounded and non-drifting** as long as the host
keeps during-speech RTF < 1 — measured ~0.44 on Apple-Silicon, i.e. roughly
2.3× slower-CPU headroom. On significantly weaker hardware the bound erodes
and lag grows; the `voice-streaming-keepup` CI workflow exists to check
keep-up on Linux and Windows runners.

Low-latency interactive streaming belongs to the Voxtral tier:
[ADR-0037](adrs/adr-0037.md) /
[#933](https://github.com/rust-works/omni-dev/issues/933) on non-Windows, and
[#936](https://github.com/rust-works/omni-dev/issues/936) (pure-Rust,
streaming-native) as the future cross-platform successor. On Windows,
interactive use rides this LCD tier until #936 lands.

### Tuning knobs

The defaults are the **recommended operating envelope** measured in #969
(`tiny.en`: RTF 0.34, WER 9.2 %, time-to-final 0.73/1.42 s mean/max, peak RSS
~429 MB) and are tuned to maximise keep-up headroom, not minimise lag. The
knobs are exposed on the Rust API only —
`CandleStreamingTranscriber::with_config(StreamingConfig { .. })` in
`src/voice/backends/candle_streaming.rs`; there are no CLI flags for them:

| Knob              | Default | Meaning                                                          |
|-------------------|---------|------------------------------------------------------------------|
| `vad_threshold`   | `0.5`   | VAD speech-score cut in `[0, 1]`; lower = more permissive         |
| `silence_secs`    | `0.3`   | Consecutive silence before an utterance endpoint; `0` disables    |
| `min_window_secs` | `2.0`   | Voiced window before the first cadence inference of a segment     |
| `cadence_secs`    | `1.0`   | New audio between re-inferences                                   |
| `max_window_secs` | `5.0`   | Hard voiced-window cap (forced flush)                             |
| `emit_partials`   | `true`  | Emit `Partial` events for the volatile tail                       |

`silence_secs` is the one knob that may need per-deployment tuning (`0.5`
cuts more conservatively). Values ≥ 0.8 are known-bad: phrase gaps stop
cutting windows, everything hits the cap mid-speech, and WER/RSS degrade
sharply (measured in the #969 sweep).

### Event semantics

The decode window holds voiced-only audio, so streaming events carry
segment-granularity times, not word alignment: `start` is when the current
utterance began, `end` is the input-audio frontier at emission, and ranges
from one utterance overlap. Deduplicate `Final`s by `event_id` (ULID,
monotonic). `Final.confidence` is the real average-logprob confidence of the
inference that committed the words.

## Validation

The streaming envelope is regression-tested against the #969 baseline by the
model-gated suite (`#[ignore]` by default — needs the model on disk and
minutes of CPU; run under `--release`):

```bash
omni-dev voice install-model
cargo test --release --test voice_streaming_candle_test -- --ignored --nocapture
```

Gates: WER ≤ 15 %, unpaced RTF ≤ 0.5, byte-identical determinism across runs,
time-to-final ≤ 2.5 s (mean & max) under a deadline-paced 1× driver, display
lag bounded and non-drifting. Partial latency is reported, not gated (the LCD
tier explicitly does not meet the interactive ≤ 1 s bar). Peak RSS is
reported on Linux and gated at ≤ 500 MB when `OMNI_DEV_STREAMING_RSS_GATE=1`
(set by the CI keep-up workflow, which runs the test in isolation).

The RTF and time-to-final gates accept env overrides
(`OMNI_DEV_STREAMING_RTF_GATE`, `OMNI_DEV_STREAMING_TTF_GATE`): the
`voice-streaming-keepup` CI lane sets them to `1.0` / `5.0` because it
checks the *keep-up* criterion (during-speech RTF < 1, bounded lag) on
hosted runners slower than target hardware, not the Apple-Silicon-calibrated
envelope. Measured: ubuntu-latest RTF 0.70 with bounded non-drifting lag
(keeps up); windows-latest RTF 1.20 (hosted Windows runners are below the
deployment floor — Windows keep-up needs validation on representative
hardware).
