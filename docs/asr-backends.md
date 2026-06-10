# ASR (Speech-to-Text) Backends

`omni-dev voice transcribe` (and the wider voice subsystem) speaks to a
**transcriber backend** chosen by `--backend`, then the
`OMNI_DEV_VOICE_BACKEND` env var, then a default. This is the ASR analogue of
the LLM backends documented in [ai-backends.md](ai-backends.md); the two are
unrelated dispatch layers.

> **Not to be confused with the AI/LLM backends.** This document is about
> *speech-to-text* runtimes. Claude/Ollama/OpenAI/Bedrock selection lives in
> [ai-backends.md](ai-backends.md).

## Backends at a glance

| Backend          | Runtime                         | Platforms                          | Default? | Opt-in |
| ---------------- | ------------------------------- | ---------------------------------- | -------- | ------ |
| `mock`           | Canned script (no model)        | all                                | yes¹     | no     |
| `whisper-candle` | Pure-Rust Whisper on `candle`   | all (macOS, Linux, Windows)        | no¹      | no     |
| `voxtral`        | Native `voxtral.c` (BF16) via FFI | macOS, Linux — **not Windows**   | no       | yes    |
| `voxtral-mlx`    | INT4 Voxtral via Apple MLX      | **macOS Apple Silicon only**       | no       | yes    |

¹ The default is `mock` until `whisper-candle` has been through a release cycle
(ADR-0033); pick a real backend explicitly with `--backend`. ADR-0035 describes
a future per-platform auto-upgrade hierarchy.

## Platform matrix

| Backend          | macOS (Apple Silicon)        | macOS (Intel)            | Linux                    | Windows                       |
| ---------------- | ---------------------------- | ------------------------ | ------------------------ | ----------------------------- |
| `mock`           | ✅                           | ✅                       | ✅                       | ✅                            |
| `whisper-candle` | ✅                           | ✅                       | ✅                       | ✅                            |
| `voxtral`        | ✅ Metal/MPS fast path       | ✅ Accelerate BLAS       | ✅ C + OpenBLAS          | ❌ excluded by design²        |
| `voxtral-mlx`    | ✅ MLX INT4 (real-time)      | ❌ Apple-only³           | ❌ Apple-only³           | ❌ Apple-only³                |

² Native Voxtral is **excluded on Windows by design** (ADR-0037): the Metal
fast path is Apple-only and the project takes on no Windows native-toolchain
requirement. `whisper-candle` is the cross-platform baseline and the **only
real ASR backend compiled on Windows**, so every platform keeps a working ASR
with no native toolchain. Requesting `--backend voxtral` on Windows (or any
build without the `voxtral` feature) is a clear construction-time error, never a
build break.

³ `voxtral-mlx` is **macOS Apple Silicon only** (ADR-0039): MLX is Apple's
framework and runs only on Apple GPUs. The `mlx-rs` dependency is declared under
a `cfg(all(target_os = "macos", target_arch = "aarch64"))` target stanza, so it
(and its C++/CMake build) never enters the Windows/Linux/Intel-macOS or default
dependency graphs. Requesting `--backend voxtral-mlx` anywhere else — or without
the `voxtral-mlx` feature — is a clear construction-time error, never a build
break.

## `whisper-candle` (cross-platform default-in-waiting)

Pure-Rust Whisper (`openai/whisper-tiny.en`) on the `candle` framework — see
[ADR-0033](adrs/adr-0033.md). No native ASR toolchain (the transitive
`onig`/`ring` C deps aside). Install the ~75 MB model once:

```sh
omni-dev voice install-model            # defaults to --variant whisper-tiny.en
omni-dev voice transcribe --backend whisper-candle audio.wav
```

Model path resolution: `--model <dir>` → `OMNI_DEV_VOICE_WHISPER_MODEL` →
`~/.omni-dev/voice/models/whisper-tiny.en/`.

## `voxtral` (native, opt-in, non-Windows)

Native [Voxtral Realtime Mini 4B](https://huggingface.co/mistralai/Voxtral-Mini-4B-Realtime-2602)
(Apache-2.0) via the vendored pure-C [`antirez/voxtral.c`](https://github.com/antirez/voxtral.c)
engine behind the `voxtral-sys` FFI crate — see [ADR-0037](adrs/adr-0037.md).
The #930 spike measured a strict streaming-quality upgrade over the Parakeet
baseline (WER ≈ 2.84–3.15 %, first-partial < 1 s).

**Building.** Off by default; enable the `voxtral` Cargo feature on a supported
target:

```sh
cargo build --features voxtral            # macOS or Linux only
```

Build requirements: a C toolchain; on **Linux** also `libopenblas-dev` and
`libclang-dev` (for `bindgen`); on **macOS** the Accelerate/Metal frameworks
and `libclang` ship with the Command Line Tools. `cmake` and C++ are **not**
required and **not** permitted (ADR-0037).

**Model.** The engine wants the full **BF16** weights — there is no INT4 path in
`voxtral.c`:

| Model                          | Backend          | On-disk size | Quantisation                         |
| ------------------------------ | ---------------- | ------------ | ------------------------------------ |
| `whisper-tiny.en`              | `whisper-candle` | ~75 MB       | fp32 safetensors                     |
| Voxtral Realtime Mini 4B       | `voxtral`        | **~8.9 GB**  | BF16 (no INT4 path in `voxtral.c`)   |
| Voxtral Realtime Mini 4B 4-bit | `voxtral-mlx`    | **~2.6 GB**  | INT4 (MLX group quantization)        |

```sh
omni-dev voice install-model --variant voxtral-mini-4b-realtime
omni-dev voice transcribe --backend voxtral --delay-ms 300 audio.wav
```

The install command is available on every host (the weights are just files);
only the *backend* that consumes them is platform-gated.

Model path resolution: `--model <dir>` → `OMNI_DEV_VOICE_VOXTRAL_MODEL` →
`~/.omni-dev/voice/models/voxtral-mini-4b-realtime/` (expects
`consolidated.safetensors`, `tekken.json`, `params.json`).

**`--delay-ms`.** The decoder delay (lookahead) in milliseconds. The #930
spike's accuracy/latency sweet spot is **240–480 ms**; the default is 480 ms.
`--backend voxtral` and `voxtral-mlx` read it; `mock` and `whisper-candle`
ignore it.

## `voxtral-mlx` (real-time INT4, opt-in, Apple Silicon)

The same Voxtral Realtime Mini 4B model, but **INT4-quantized and run through
Apple [MLX](https://github.com/ml-explore/mlx)** (via the `mlx-rs` crate) — a
pure-Rust port of the model's forward pass, not a wrapper around `voxtral.c`. See
[ADR-0039](adrs/adr-0039.md). INT4 is the lever that makes Voxtral **real-time**:
LLM decode is memory-bandwidth-bound, and 4-bit weights move ≈ 4× less data than
the BF16 `voxtral.c` path.

Measured on Apple Silicon (release build, against the 5-minute fixture):

| Path      | WER   | RTF                | First-Partial |
| --------- | ----- | ------------------ | ------------- |
| batch     | 4.0 % | **0.263** (~4× RT) | —             |
| streaming | 4.6 % | real-time          | **1.34 s**    |

For comparison the BF16 `voxtral.c` path measured WER ≈ 4.1 % at **RTF 1.25**
(slower than real-time) with a first-Partial of ≈ 2.7 s — so `voxtral-mlx` matches
its accuracy while being ~5× faster and halving first-Partial latency.

**Building.** Off by default; enable the `voxtral-mlx` Cargo feature **on macOS
Apple Silicon**:

```sh
cargo build --features voxtral-mlx        # macOS arm64 only
```

Build requirements: **CMake and a C++ toolchain** — `mlx-rs` compiles the MLX C++
core from source. This is the cost [ADR-0039](adrs/adr-0039.md) narrowly permits
for this backend (the project is otherwise C++/CMake-free; `voxtral` uses only
`cc`). MLX also needs Xcode's **Metal Toolchain** component for ahead-of-time
shader compilation (`xcodebuild -downloadComponent MetalToolchain` if the build
reports a missing `metal` compiler).

**Model.** The INT4 weights (~2.6 GB, Apache-2.0,
[`mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit`](https://huggingface.co/mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit)):

```sh
omni-dev voice install-model --variant voxtral-mlx-int4
omni-dev voice transcribe --backend voxtral-mlx audio.wav
omni-dev voice listen --backend voxtral-mlx          # real-time streaming
```

The install command runs on any host (the weights are just files); only the
*backend* is Apple-Silicon-gated.

Model path resolution: `--model <dir>` → `OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL` →
`~/.omni-dev/voice/models/voxtral-mlx-int4/` (expects `model.safetensors` and
`tekken.json`).

## See also

- [ADR-0033](adrs/adr-0033.md) — `candle` as the production ASR runtime.
- [ADR-0035](adrs/adr-0035.md) — OS-gated ASR backends and the factory.
- [ADR-0037](adrs/adr-0037.md) — pure-C native ASR backends behind a Rust FFI
  boundary on non-Windows targets.
- [ADR-0039](adrs/adr-0039.md) — permitting C++/CMake (via `mlx-rs`) for the
  real-time INT4 Voxtral MLX backend, Apple-Silicon-only.
