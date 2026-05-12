//! Voice capture: microphone-to-WAV pipeline.
//!
//! The library half of `omni-dev voice capture`. The CLI entry point lives in
//! [`crate::cli::voice`]. This module is intentionally CLI-free so the audio
//! pipeline (source → mixdown → resample → idle-detect → trim → write) can be
//! unit-tested against fixture WAVs without a real microphone.
//!
//! The `AudioSource` trait in [`mod@audio`] is the test seam: production code
//! uses [`audio::CpalAudioSource`], tests use [`audio::FileAudioSource`].
//! See [ADR-0031](../../docs/adrs/adr-0031.md) for the rationale.

pub mod audio;
pub mod capture;
pub mod idle;
pub mod wav;
