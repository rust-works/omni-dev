//! Real-time INT4 Voxtral backend via Apple **MLX** (`mlx-rs`) — [ADR-0039].
//!
//! Compiled only with the off-by-default `voxtral-mlx` feature on macOS Apple
//! Silicon (MLX is Apple-only). This is a **port** of `mlx-audio`'s Python
//! `voxtral_realtime` model to `mlx-rs`, running the INT4-quantized weights for
//! real-time transcription (the BF16 `voxtral.c` path measured RTF 1.25; INT4
//! is the lever — #933 validation).
//!
//! **Status (#933 M1 complete).** The full offline forward pass is ported and
//! **validated end-to-end**: mel front-end (M1.4) → encoder + adapter (M1.2) →
//! decoder with GQA + ada-norm + KV cache (M1.3) → Tekken decode (M1.4), wired by
//! [`VoxtralMlxModel::transcribe`] (M1.4). On the real INT4 model on Metal it
//! produces correct transcripts; on a 32 s prefix of the 5-min fixture (release)
//! it measured **WER 1.5%** and **RTF 0.193** (≈ 5× real-time) — versus the
//! `voxtral.c` BF16 path's RTF 1.25, confirming ADR-0039's INT4 real-time thesis
//! (M1.5). The batch backend [`VoxtralMlxBackend`] implements [`Transcriber`] and
//! is wired to `--backend voxtral-mlx` with a `voxtral-mlx-int4` install variant
//! (M2). Remaining: long-audio chunking + the `StreamingTranscriber` (M3), CI
//! graph-gating, and docs/security/`voxtral.c` fate (M4).
//!
//! [`Transcriber`]: crate::voice::Transcriber
//!
//! [ADR-0039]: ../../../../docs/adrs/adr-0039.md
// Port in progress (#933 M1): layers consume the config/weights incrementally,
// and the config field docs are filled in as the structs stabilise. Both allows
// are removed when the backend is complete (M1.5).
#![allow(dead_code, missing_docs)]

mod backend;
mod config;
mod decoder;
mod encoder;
mod mel;
mod model;
mod nn;
mod tokenizer;
mod weights;

pub use backend::{VoxtralMlxBackend, DEFAULT_VOXTRAL_MLX_DELAY_MS};
pub use config::VoxtralMlxConfig;
pub use decoder::{Decoder, KvCache};
pub use encoder::AudioEncoder;
pub use model::{load_wav_16k_mono, VoxtralMlxModel};
pub use nn::Weights;
pub use tokenizer::TekkenTokenizer;
pub use weights::{get_tensor, load_safetensors};
