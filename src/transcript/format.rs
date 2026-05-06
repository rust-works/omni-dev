//! Source-agnostic transcript output formats.
//!
//! [`Format`] is the user-facing format selector; per-format converters
//! ([`srt`], [`vtt`], [`txt`], [`json`]) take `&[Cue]` so they can be reused
//! by any [`TranscriptSource`](crate::transcript::source::TranscriptSource).

use std::fmt;
use std::str::FromStr;

use crate::transcript::error::{Result, TranscriptError};
use crate::transcript::source::Transcript;

pub mod json;
pub mod srt;
pub mod txt;
pub mod vtt;

/// Output formats supported by `omni-dev transcript … fetch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// SubRip (`.srt`) — sequence-numbered cues with `HH:MM:SS,mmm` timecodes.
    Srt,
    /// WebVTT (`.vtt`) — `WEBVTT` header followed by cues with
    /// `HH:MM:SS.mmm` timecodes.
    Vtt,
    /// Plain text — cue text only, one cue per line, no timing.
    Txt,
    /// JSON — the full [`Transcript`] struct serialised via serde.
    Json,
}

impl Format {
    /// All variants in declaration order. Useful for help output and tests.
    pub const ALL: &'static [Self] = &[Self::Srt, Self::Vtt, Self::Txt, Self::Json];

    /// Lowercase, file-extension-style identifier (`"srt"`, `"vtt"`,
    /// `"txt"`, `"json"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::Txt => "txt",
            Self::Json => "json",
        }
    }

    /// Render `transcript` to the format's textual representation.
    ///
    /// Errors only for [`Format::Json`], which can fail if the transcript
    /// contains values serde rejects (in practice unreachable for the
    /// [`Transcript`] type, but the `Result` keeps the API uniform if other
    /// formats grow fallible behaviour later).
    pub fn render(self, transcript: &Transcript) -> Result<String> {
        match self {
            Self::Srt => Ok(srt::render(&transcript.cues)),
            Self::Vtt => Ok(vtt::render(&transcript.cues)),
            Self::Txt => Ok(txt::render(&transcript.cues)),
            Self::Json => json::render(transcript),
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Format {
    type Err = TranscriptError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "srt" => Ok(Self::Srt),
            "vtt" => Ok(Self::Vtt),
            "txt" | "text" | "plain" => Ok(Self::Txt),
            "json" => Ok(Self::Json),
            other => Err(TranscriptError::ParseError(format!(
                "unknown transcript format `{other}`; expected one of: srt, vtt, txt, json"
            ))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::transcript::cue::Cue;
    use crate::transcript::source::TrackKind;

    fn sample_transcript() -> Transcript {
        Transcript {
            source: "mock".into(),
            locator_id: "abc".into(),
            language: "en".into(),
            kind: TrackKind::Manual,
            cues: vec![Cue::new(0, 1000, "hello"), Cue::new(1000, 2500, "world")],
        }
    }

    #[test]
    fn as_str_round_trips_for_canonical_names() {
        for &f in Format::ALL {
            let parsed: Format = f.as_str().parse().unwrap();
            assert_eq!(parsed, f);
        }
    }

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!(Format::from_str("SRT").unwrap(), Format::Srt);
        assert_eq!(Format::from_str("Vtt").unwrap(), Format::Vtt);
        assert_eq!(Format::from_str("JSON").unwrap(), Format::Json);
    }

    #[test]
    fn from_str_accepts_txt_aliases() {
        assert_eq!(Format::from_str("txt").unwrap(), Format::Txt);
        assert_eq!(Format::from_str("text").unwrap(), Format::Txt);
        assert_eq!(Format::from_str("plain").unwrap(), Format::Txt);
    }

    #[test]
    fn from_str_unknown_errors_with_helpful_message() {
        let err = Format::from_str("yaml").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("yaml"));
        assert!(msg.contains("srt"));
        assert!(msg.contains("vtt"));
        assert!(msg.contains("txt"));
        assert!(msg.contains("json"));
    }

    #[test]
    fn display_matches_as_str() {
        for &f in Format::ALL {
            assert_eq!(format!("{f}"), f.as_str());
        }
    }

    #[test]
    fn render_dispatches_to_srt() {
        let out = Format::Srt.render(&sample_transcript()).unwrap();
        assert!(out.contains("00:00:00,000 --> 00:00:01,000"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn render_dispatches_to_vtt() {
        let out = Format::Vtt.render(&sample_transcript()).unwrap();
        assert!(out.starts_with("WEBVTT"));
        assert!(out.contains("00:00:00.000 --> 00:00:01.000"));
    }

    #[test]
    fn render_dispatches_to_txt() {
        let out = Format::Txt.render(&sample_transcript()).unwrap();
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn render_dispatches_to_json() {
        let out = Format::Json.render(&sample_transcript()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["language"], "en");
        assert_eq!(value["cues"][0]["text"], "hello");
    }

    #[test]
    fn all_constant_lists_each_variant_once() {
        assert_eq!(Format::ALL.len(), 4);
        let mut copy: Vec<Format> = Format::ALL.to_vec();
        copy.sort_by_key(|f| f.as_str());
        copy.dedup();
        assert_eq!(copy.len(), 4);
    }
}
