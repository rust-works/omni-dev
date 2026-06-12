//! `Transcriber` backends.
//!
//! Each backend is a concrete implementation of
//! [`crate::voice::Transcriber`] dispatched through
//! [`crate::voice::factory::create_default_transcriber`]. Backend choice
//! is steered by `--backend` / `OMNI_DEV_VOICE_BACKEND`.
//!
//! Three backends are wired up:
//!
//! - [`mock::MockTranscriber`] — canned-script placeholder (default).
//! - [`candle::CandleTranscriber`] — pure-Rust Whisper on `candle`
//!   (`--backend whisper-candle`). See ADR-0033.
//! - [`candle_streaming::CandleStreamingTranscriber`] — pure-Rust
//!   streaming Whisper with VAD chunking + LocalAgreement-2
//!   (`--backend whisper-candle-streaming`). Latency-tolerant LCD tier;
//!   see ADR-0040.

pub mod candle;
pub mod candle_streaming;
pub mod mock;
