//! The [`Cue`] value type — a single timed text segment.

use serde::{Deserialize, Serialize};

/// A single timed caption/subtitle segment.
///
/// Times are in milliseconds from the start of the media. `end_ms` is the
/// inclusive on-screen end of the cue; `end_ms == start_ms` represents an
/// instantaneous cue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    /// Cue start time in milliseconds.
    pub start_ms: u64,
    /// Cue end time in milliseconds.
    pub end_ms: u64,
    /// The text shown for this cue. May contain newlines.
    pub text: String,
}

impl Cue {
    /// Construct a new cue.
    pub fn new(start_ms: u64, end_ms: u64, text: impl Into<String>) -> Self {
        Self {
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    /// On-screen duration of the cue in milliseconds. Saturates at zero if
    /// `end_ms < start_ms` (which should not occur in well-formed input but
    /// can be encountered in adversarial captions data).
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn new_constructs_cue() {
        let cue = Cue::new(0, 1000, "hello");
        assert_eq!(cue.start_ms, 0);
        assert_eq!(cue.end_ms, 1000);
        assert_eq!(cue.text, "hello");
    }

    #[test]
    fn new_accepts_string_and_str() {
        let from_str = Cue::new(0, 100, "x");
        let from_string = Cue::new(0, 100, String::from("x"));
        assert_eq!(from_str, from_string);
    }

    #[test]
    fn duration_ms_basic() {
        let cue = Cue::new(500, 1500, "x");
        assert_eq!(cue.duration_ms(), 1000);
    }

    #[test]
    fn duration_ms_zero_length() {
        let cue = Cue::new(500, 500, "x");
        assert_eq!(cue.duration_ms(), 0);
    }

    #[test]
    fn duration_ms_saturates_when_inverted() {
        let cue = Cue::new(2000, 1000, "x");
        assert_eq!(cue.duration_ms(), 0);
    }

    #[test]
    fn equality_compares_all_fields() {
        let a = Cue::new(0, 100, "hi");
        let b = Cue::new(0, 100, "hi");
        let c = Cue::new(0, 100, "bye");
        let d = Cue::new(1, 100, "hi");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn serde_round_trip_json() {
        let cue = Cue::new(1234, 5678, "hello\nworld");
        let json = serde_json::to_string(&cue).expect("serialize");
        let back: Cue = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cue, back);
    }

    #[test]
    fn serde_field_names_are_snake_case() {
        let cue = Cue::new(1, 2, "x");
        let json = serde_json::to_value(&cue).expect("serialize");
        assert!(json.get("start_ms").is_some());
        assert!(json.get("end_ms").is_some());
        assert!(json.get("text").is_some());
    }

    #[test]
    fn debug_impl_present() {
        let cue = Cue::new(0, 100, "hi");
        let dbg = format!("{cue:?}");
        assert!(dbg.contains("Cue"));
        assert!(dbg.contains("hi"));
    }

    #[test]
    fn clone_independent() {
        let a = Cue::new(0, 100, "hi");
        let b = a.clone();
        assert_eq!(a, b);
    }
}
