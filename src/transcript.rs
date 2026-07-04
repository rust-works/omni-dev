//! Transcript and caption fetching from media platforms.
//!
//! Provides a source-agnostic library for retrieving transcripts/captions from
//! external media platforms (YouTube first; Vimeo, podcasts, generic VTT/SRT
//! URLs to follow). The [`source::TranscriptSource`] trait is the extension
//! point — concrete sources live under [`sources`], format converters under
//! [`mod@format`], and shared value types ([`cue::Cue`], [`source::Transcript`])
//! are reused across all sources.
//!
//! This module has no `clap` dependency and is reusable from other commands or
//! external consumers.

pub mod cue;
pub mod detect;
pub mod error;
pub mod format;
pub mod source;
pub mod sources;

pub use cue::Cue;
pub use detect::detect;
pub use error::{Result, TranscriptError};
pub use format::Format;
pub use source::{FetchOpts, LanguageInfo, MediaInfo, TrackKind, Transcript, TranscriptSource};
