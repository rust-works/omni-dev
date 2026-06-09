//! Real-time INT4 Voxtral backend via Apple **MLX** (`mlx-rs`) — [ADR-0039].
//!
//! Compiled only with the off-by-default `voxtral-mlx` feature on macOS Apple
//! Silicon (MLX is Apple-only). This is a **port** of `mlx-audio`'s Python
//! `voxtral_realtime` model to `mlx-rs`, running the INT4-quantized weights for
//! real-time transcription (the BF16 `voxtral.c` path measured RTF 1.25; INT4
//! is the lever — #933 validation).
//!
//! **Under construction (#933 M1).** The full offline forward pass is ported and
//! **verified end-to-end**: mel front-end (M1.4) → encoder + adapter (M1.2) →
//! decoder with GQA + ada-norm + KV cache (M1.3) → Tekken decode (M1.4), wired by
//! [`VoxtralMlxModel::transcribe`] (M1.4) — it produces a correct transcript from
//! the real INT4 model on Metal (M1.5). Remaining: the `Transcriber` impl + model
//! management (M2), long-audio chunking + the `StreamingTranscriber` (M3), and
//! release-build RTF/WER metrics (M4).
//!
//! [ADR-0039]: ../../../../docs/adrs/adr-0039.md
// Port in progress (#933 M1): layers consume the config/weights incrementally,
// and the config field docs are filled in as the structs stabilise. Both allows
// are removed when the backend is complete (M1.5).
#![allow(dead_code, missing_docs)]

mod config;
mod decoder;
mod encoder;
mod mel;
mod model;
mod nn;
mod tokenizer;
mod weights;

pub use config::VoxtralMlxConfig;
pub use decoder::{Decoder, KvCache};
pub use encoder::AudioEncoder;
pub use model::{load_wav_16k_mono, VoxtralMlxModel};
pub use nn::Weights;
pub use tokenizer::TekkenTokenizer;
pub use weights::{get_tensor, load_safetensors};
