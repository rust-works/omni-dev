# Voxtral.c build-feasibility probe (#930, Step 4)

Source: [`antirez/voxtral.c`](https://github.com/antirez/voxtral.c), cloned to
`repo/` (gitignored). Goal: a feasibility verdict for the "C-FFI / port"
integration path — **not** full latency/WER measurements (those come from the
mlx-audio path).

## Build result — PASS

Host: Apple Silicon, Apple clang 17.0.0. Both backends compile clean:

| Target | Wall | Output | Toolchain | Deps |
|---|---|---|---|---|
| `make blas` | **2.6 s** | `voxtral` (124 KB) | gcc/clang, pure C | `-framework Accelerate AudioToolbox CoreFoundation` (system) |
| `make mps`  | **2.7 s** | `voxtral` (246 KB) | clang C **+ Objective-C** (`voxtral_metal.m`, `-fobjc-arc`) | `+ Metal MetalPerformanceShaders MetalPerformanceShadersGraph Foundation` |

`./voxtral -h` runs and prints usage. No model was downloaded (the C impl wants
the **~8.9 GB bf16** weights via `download_model.sh`, not the 3 GB INT4 MLX
quant), so end-to-end transcription was **not** run — deliberate budget call;
build + API inspection is sufficient signal for the integration ranking.

## Language / ADR-0033 (C++-freeness)

- **Zero C++** (`.cpp`/`.cc`/`.hpp`) in the tree — all `.c`/`.h`.
- **BLAS path is pure C** + Accelerate (a C-API system framework). This is the
  ADR-0033-compatible path (ADR-0033 forbids C++ for ASR; C + system frameworks
  is in-bounds).
- **MPS path adds Objective-C** (`voxtral_metal.m`) + Metal shaders. Objective-C
  is not C++, but it is a new toolchain surface ADR-0033 doesn't currently speak
  to; would need an explicit ADR note if the fast path is adopted.
- README: BLAS is "usable but slow (continuously converts bf16 weights to fp32)";
  MPS is the fast path. So the ADR-clean path is also the slow one.

## FFI surface (`voxtral.h`) — clean and Rust-bindable

A streaming C API maps almost 1:1 onto the mlx-audio session API we measured:

```c
vox_stream_t *vox_stream_init(vox_ctx_t *ctx);
int  vox_stream_feed(vox_stream_t *s, const float *samples, int n_samples); // 16k mono f32
int  vox_stream_finish(vox_stream_t *s);
int  vox_stream_get(vox_stream_t *s, const char **out_tokens, int max);     // token strings
void vox_set_processing_interval(vox_stream_t *s, float seconds);           // latency knob (~= -I)
void vox_stream_free(vox_stream_t *s);
```

Plain C structs + float buffers + `const char*` token strings → trivial `bindgen`
target, no C++ name-mangling/ABI issues. Also exposes `--stdin` (raw s16le 16k
mono) for a zero-FFI subprocess option, and `--from-mic` (macOS).

## Cons for the integration decision

- **~8.9 GB bf16 model** (no INT4 path) vs the MLX 3 GB INT4 → ~3× the disk and
  a much larger RSS than the 4.55 GB we measured for the MLX INT4 path.
- Fast path requires **Objective-C + Metal** (new build surface, macOS-only).
- Author's own caveat: "more testing needed … likely requires some more work to
  be production quality."
- Vendoring a 3rd-party C engine = ongoing maintenance + security-review surface.
