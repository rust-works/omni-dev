//! Parakeet-TDT-0.6B-v2 backend on `candle`.
//!
//! Pure-Rust port of `mlx-community/parakeet-tdt-0.6b-v2` against the
//! `candle 0.10.x` runtime — FastConformer encoder + TDT decoder + joiner,
//! 600 M params, English-only ASR. Lands in stages per the issue #898
//! commit plan; this module currently exposes the weights loader and
//! mel-spectrogram front-end (commit 3). The encoder, decoder, tokenizer,
//! streaming wrapper, and public `Transcriber` surface land in subsequent
//! commits.
//!
//! Architecture rationale: ADR-0033 (candle for ASR), and the #871
//! feasibility spike's GO recommendation.

pub mod attention;
pub mod audio;
pub mod conv_module;
pub mod weights;
