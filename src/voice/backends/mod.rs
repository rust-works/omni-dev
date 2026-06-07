//! `Transcriber` backends.
//!
//! Each backend is a concrete implementation of
//! [`crate::voice::Transcriber`] dispatched through
//! [`crate::voice::factory::create_default_transcriber`]. Backend choice
//! is steered by `--backend` / `OMNI_DEV_VOICE_BACKEND`.
//!
//! Backends:
//!
//! - [`mock::MockTranscriber`] — canned-script placeholder (default).
//! - [`candle::CandleTranscriber`] — pure-Rust Whisper on `candle`
//!   (`--backend whisper-candle`). See ADR-0033.
//! - `voxtral::VoxtralBackend` — native Voxtral Realtime via the `voxtral-sys`
//!   FFI engine (`--backend voxtral`). Compiled only with the off-by-default
//!   `voxtral` feature on `cfg(not(target_os = "windows"))`. See ADR-0037.

pub mod candle;
pub mod mock;

#[cfg(all(feature = "voxtral", not(target_os = "windows")))]
pub mod voxtral;
