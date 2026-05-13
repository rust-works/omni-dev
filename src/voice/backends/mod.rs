//! `Transcriber` backends.
//!
//! Each backend is a concrete implementation of
//! [`crate::voice::Transcriber`] dispatched through
//! [`crate::voice::factory::create_default_transcriber`]. Backend choice
//! is steered by `--backend` / `OMNI_DEV_VOICE_BACKEND`.
//!
//! Currently only [`mock::MockTranscriber`] is wired up — the real
//! Whisper-based batch backend originally scoped in #801 was deferred to
//! its own issue when the no-C++-dependencies constraint surfaced. The
//! trait surface and event types from #801 still land here; the inference
//! backend slots in via the same `Transcriber` trait without changes to
//! callers.

pub mod mock;
