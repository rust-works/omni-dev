//! Real-time INT4 Voxtral backend via Apple **MLX** (`mlx-rs`) — [ADR-0039].
//!
//! Compiled only with the off-by-default `voxtral-mlx` feature on macOS Apple
//! Silicon (MLX is Apple-only). This is a **port** of `mlx-audio`'s Python
//! `voxtral_realtime` model to `mlx-rs`, running the INT4-quantized weights for
//! real-time transcription (the BF16 `voxtral.c` path measured RTF 1.25; INT4
//! is the lever — #933 validation).
//!
//! **Under construction (#933 M1).** This currently provides the config and
//! weights loader (M1.1), the causal audio encoder + adapter (M1.2), and the LLM
//! decoder with GQA + ada-norm + a growing KV cache (M1.3) — encoder and decoder
//! forwards both verified running on the real INT4 weights on Metal. The Tekken
//! tokenizer + mel front-end + the generation loop (M1.4) and the
//! `Transcriber`/`StreamingTranscriber` impls (M2/M3) land next.
//!
//! [ADR-0039]: ../../../../docs/adrs/adr-0039.md
// Port in progress (#933 M1): layers consume the config/weights incrementally,
// and the config field docs are filled in as the structs stabilise. Both allows
// are removed when the backend is complete (M1.5).
#![allow(dead_code, missing_docs)]

mod config;
mod decoder;
mod encoder;
mod nn;
mod weights;

pub use config::VoxtralMlxConfig;
pub use decoder::{Decoder, KvCache};
pub use encoder::AudioEncoder;
pub use nn::Weights;
pub use weights::{get_tensor, load_safetensors};
