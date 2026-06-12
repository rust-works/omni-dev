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
pub mod features;
pub mod idle;
pub mod models;
pub mod paths;
pub mod reconcile;
pub mod reflect;
pub mod render;
pub mod review;
pub mod session;
pub mod speaker;
pub mod transcriber;
pub mod vad;
pub mod wav;

pub use audio::{AudioSource, CpalAudioSource, FileAudioSource};
pub use capture::{
    install_ctrl_c_handler, run_capture, CaptureOpts, CaptureSummary, TerminationReason,
};
pub use clock::{Clock, FixedClock, SystemClock};
pub use det::{CountingUlidRng, SystemUlidRng, UlidRng};
pub use factory::{create_default_transcriber, VoiceOpts};
pub use paths::{captures_dir, omni_dev_voice_root, speaker_file, speakers_dir};
pub use render::{detect_format, render_jsonl, render_markdown, OutputFormat};
pub use speaker::{cosine, l2_normalise, EnrolledSpeaker, WespeakerEmbedder, MIN_EMBED_SAMPLES};
pub use transcriber::{
    AudioChunk, AudioInput, EndpointKind, EventId, EventStream, SpeakerId, Transcriber,
    TranscriptEvent, VecAudioInput, Word,
};
