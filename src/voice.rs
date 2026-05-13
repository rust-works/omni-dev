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
pub mod backends;
pub mod capture;
pub mod clock;
pub mod det;
pub mod events;
pub mod factory;
pub mod idle;
pub mod models;
pub mod reflect;
pub mod render;
pub mod session;
pub mod transcriber;
pub mod wav;

pub use audio::{AudioSource, CpalAudioSource, FileAudioSource};
pub use capture::{
    install_ctrl_c_handler, run_capture, CaptureOpts, CaptureSummary, TerminationReason,
};
pub use clock::{Clock, FixedClock, SystemClock};
pub use det::{CountingUlidRng, SystemUlidRng, UlidRng};
pub use factory::{create_default_transcriber, VoiceOpts};
pub use render::{detect_format, render_jsonl, render_markdown, OutputFormat};
pub use transcriber::{
    AudioChunk, AudioInput, EndpointKind, EventId, EventStream, SpeakerId, Transcriber,
    TranscriptEvent, VecAudioInput, Word,
};
