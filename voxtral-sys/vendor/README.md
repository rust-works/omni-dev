# Vendored `voxtral.c`

This directory contains a **committed copy** of the pure-C Voxtral inference
engine, vendored behind the `voxtral-sys` FFI crate per
[ADR-0037](../../docs/adrs/adr-0037.md) and issue
[#933](https://github.com/rust-works/omni-dev/issues/933).

A committed copy (rather than a git submodule) is used deliberately: submodules
do not package into a crates.io tarball and complicate CI checkouts. This mirrors
the standard `*-sys` crate convention (e.g. `openssl-sys`, `zstd-sys`).

## Provenance

| | |
| --- | --- |
| Upstream | <https://github.com/antirez/voxtral.c> |
| Pinned commit | `134d366c24d20c64b614a3dcc8bda2a6922d077d` |
| Code license | MIT — © 2026 Salvatore Sanfilippo (see [`voxtral.c/LICENSE`](voxtral.c/LICENSE)) |
| Model license | Apache-2.0 (weights are **not** vendored — fetched separately) |

The MIT license is on the project's allowlist in [`deny.toml`](../../deny.toml).

## What is vendored

Only the **library** translation units and their headers are vendored — the
upstream CLI (`main.c`), the live-microphone capture (`voxtral_mic_macos.c`,
`voxtral_mic.h`), and the weight-inspector utility (`inspect_weights.c`) are
**excluded**: `voxtral-sys` feeds audio samples through `vox_stream_feed` and has
no use for them, and excluding them keeps the security-review surface (ADR-0037)
minimal.

**Compiled library TUs**

- `voxtral.c`, `voxtral_kernels.c`, `voxtral_audio.c`, `voxtral_encoder.c`,
  `voxtral_decoder.c`, `voxtral_tokenizer.c`, `voxtral_safetensors.c`
- `voxtral_metal.m` (Objective-C) — compiled only on the macOS Metal/MPS path
- `voxtral_shaders.metal` — embedded as a C string by `build.rs` (the
  `voxtral_shaders_source.h` the Metal path `#include`s is generated into
  `OUT_DIR`, not vendored)

**Headers**: `voxtral.h` (public API), `voxtral_audio.h`, `voxtral_kernels.h`,
`voxtral_metal.h`, `voxtral_safetensors.h`, `voxtral_tokenizer.h`.

All `#include "…"` directives in the compiled sources resolve within this
directory, except `voxtral_shaders_source.h` (build-generated, Metal path only).

## Build deltas vs upstream's Makefile

`build.rs` drives the build via the `cc` crate rather than the upstream
`Makefile`. The one intentional flag difference: **`-march=native` is dropped**.
Upstream uses it for local builds, but it produces binaries that `SIGILL` on a
different CPU than the one that compiled them — unacceptable for a distributable
artifact. `-O3 -ffast-math` are kept. Backend selection (`USE_BLAS` /
`USE_METAL` / OpenBLAS vs Accelerate) follows the upstream Makefile's per-platform
logic; see [`../build.rs`](../build.rs).

## Re-vendoring procedure

```sh
git clone https://github.com/antirez/voxtral.c /tmp/voxtral-upstream
cd /tmp/voxtral-upstream && git checkout <new-commit>
# Copy the library TUs + headers + LICENSE listed above into voxtral.c/,
# then bump the pinned commit in this file and re-run `cargo test -p voxtral-sys`.
```

Re-vendoring is a **security-relevant** change: it pulls new third-party native
code into the trust boundary and must go through the `security-review` ADR-0037
mandates before shipping.
