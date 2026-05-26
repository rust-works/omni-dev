//! `Transcriber` backends.
//!
//! Each backend is a concrete implementation of
//! [`crate::voice::Transcriber`] dispatched through
//! [`crate::voice::factory::create_default_transcriber`]. Backend choice
//! is steered by `--backend` / `OMNI_DEV_VOICE_BACKEND`.
//!
//! Two backends are wired up:
//!
//! - [`mock::MockTranscriber`] — canned-script placeholder (default).
//! - [`candle::CandleTranscriber`] — pure-Rust Whisper on `candle`
//!   (`--backend whisper-candle`). See ADR-0033.

pub mod candle;
pub mod mock;
pub mod parakeet;
