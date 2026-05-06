//! JSON renderer — pretty-printed serialisation of the full
//! [`Transcript`](crate::transcript::source::Transcript) struct.

use crate::transcript::error::{Result, TranscriptError};
use crate::transcript::source::Transcript;

/// Serialise `transcript` to a pretty-printed JSON string.
///
/// Errors only if `serde_json::to_string_pretty` fails (in practice
/// unreachable for `Transcript`, since every field is JSON-serialisable,
/// but the API returns `Result` to keep the contract uniform across
/// formats).
pub fn render(transcript: &Transcript) -> Result<String> {
    serde_json::to_string_pretty(transcript)
        .map(|s| {
            let mut s = s;
            s.push('\n');
            s
        })
        .map_err(|e| TranscriptError::ParseError(format!("json serialisation failed: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::transcript::cue::Cue;
    use crate::transcript::source::TrackKind;

    fn fixture(cues: Vec<Cue>) -> Transcript {
        Transcript {
            source: "mock".into(),
            locator_id: "abc".into(),
            language: "en".into(),
            kind: TrackKind::Manual,
            cues,
        }
    }

    #[test]
    fn empty_cues_serialises() {
        let out = render(&fixture(vec![])).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["cues"], serde_json::json!([]));
    }

    #[test]
    fn includes_top_level_metadata() {
        let out = render(&fixture(vec![])).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["source"], "mock");
        assert_eq!(v["locator_id"], "abc");
        assert_eq!(v["language"], "en");
        assert_eq!(v["kind"], "manual");
    }

    #[test]
    fn cues_have_timing_fields() {
        let out = render(&fixture(vec![Cue::new(123, 456, "hi")])).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let cue = &v["cues"][0];
        assert_eq!(cue["start_ms"], 123);
        assert_eq!(cue["end_ms"], 456);
        assert_eq!(cue["text"], "hi");
    }

    #[test]
    fn output_is_pretty_printed() {
        let out = render(&fixture(vec![Cue::new(0, 1, "x")])).unwrap();
        // Pretty output has indentation and one field per line.
        assert!(out.contains("\n  "));
    }

    #[test]
    fn output_ends_with_trailing_newline() {
        let out = render(&fixture(vec![])).unwrap();
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn round_trips_through_serde() {
        let original = fixture(vec![
            Cue::new(0, 1_000, "hello"),
            Cue::new(1_000, 2_500, "world\nlines"),
        ]);
        let out = render(&original).unwrap();
        let back: Transcript = serde_json::from_str(&out).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn track_kind_translated_serialised_lowercase() {
        let mut t = fixture(vec![]);
        t.kind = TrackKind::Translated;
        let out = render(&t).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["kind"], "translated");
    }

    #[test]
    fn special_characters_in_text_are_escaped() {
        let t = fixture(vec![Cue::new(0, 1, "quote \" backslash \\ newline\n")]);
        let out = render(&t).unwrap();
        // The raw output contains escape sequences; deserialising restores
        // the original bytes.
        let back: Transcript = serde_json::from_str(&out).unwrap();
        assert_eq!(back.cues[0].text, "quote \" backslash \\ newline\n");
    }
}
