//! SubRip (`.srt`) renderer.
//!
//! ```text
//! 1
//! 00:00:01,000 --> 00:00:04,000
//! Subtitle text
//!
//! 2
//! ...
//! ```

use std::fmt::Write;

use crate::transcript::cue::Cue;

/// Render `cues` to a SubRip-formatted string. Returns `""` for an empty
/// input.
pub fn render(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (idx, cue) in cues.iter().enumerate() {
        let _ = writeln!(out, "{}", idx + 1);
        let _ = writeln!(
            out,
            "{} --> {}",
            format_timestamp(cue.start_ms),
            format_timestamp(cue.end_ms)
        );
        out.push_str(&cue.text);
        out.push('\n');
        out.push('\n');
    }
    out
}

/// Format `ms` as `HH:MM:SS,mmm` (SRT's comma decimal separator).
fn format_timestamp(ms: u64) -> String {
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    let millis = ms % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(render(&[]), "");
    }

    #[test]
    fn single_cue_basic() {
        let cues = vec![Cue::new(1_000, 4_000, "Hello, world.")];
        let out = render(&cues);
        assert_eq!(out, "1\n00:00:01,000 --> 00:00:04,000\nHello, world.\n\n");
    }

    #[test]
    fn multiple_cues_are_sequentially_numbered() {
        let cues = vec![
            Cue::new(0, 1_000, "one"),
            Cue::new(1_000, 2_000, "two"),
            Cue::new(2_000, 3_000, "three"),
        ];
        let out = render(&cues);
        assert!(out.starts_with("1\n"));
        assert!(out.contains("\n2\n"));
        assert!(out.contains("\n3\n"));
        assert!(out.contains("one"));
        assert!(out.contains("two"));
        assert!(out.contains("three"));
    }

    #[test]
    fn timestamps_pad_zero() {
        assert_eq!(format_timestamp(0), "00:00:00,000");
    }

    #[test]
    fn timestamps_handle_subsecond_millis() {
        assert_eq!(format_timestamp(1), "00:00:00,001");
        assert_eq!(format_timestamp(999), "00:00:00,999");
    }

    #[test]
    fn timestamps_handle_minute_boundary() {
        assert_eq!(format_timestamp(60_000), "00:01:00,000");
        assert_eq!(format_timestamp(59_999), "00:00:59,999");
    }

    #[test]
    fn timestamps_handle_hour_boundary() {
        assert_eq!(format_timestamp(3_600_000), "01:00:00,000");
        assert_eq!(format_timestamp(3_599_999), "00:59:59,999");
    }

    #[test]
    fn timestamps_handle_multi_hour() {
        assert_eq!(
            format_timestamp(2 * 3_600_000 + 30 * 60_000 + 45 * 1_000 + 678),
            "02:30:45,678"
        );
    }

    #[test]
    fn timestamps_use_comma_separator_not_period() {
        let ts = format_timestamp(1_500);
        assert!(ts.contains(','));
        assert!(!ts.contains('.'));
    }

    #[test]
    fn cue_text_with_newlines_is_preserved() {
        let cues = vec![Cue::new(0, 1_000, "line one\nline two")];
        let out = render(&cues);
        assert!(out.contains("line one\nline two\n\n"));
    }

    #[test]
    fn zero_length_cue_is_emitted() {
        let cues = vec![Cue::new(500, 500, "instant")];
        let out = render(&cues);
        assert!(out.contains("00:00:00,500 --> 00:00:00,500"));
        assert!(out.contains("instant"));
    }

    #[test]
    fn empty_text_cue_still_has_blank_line() {
        let cues = vec![Cue::new(0, 1_000, "")];
        let out = render(&cues);
        // Cue index, timecode, empty content, blank separator.
        assert_eq!(out, "1\n00:00:00,000 --> 00:00:01,000\n\n\n");
    }

    #[test]
    fn output_ends_with_blank_line_separator() {
        let cues = vec![Cue::new(0, 1_000, "x")];
        let out = render(&cues);
        assert!(out.ends_with("\n\n"));
    }

    #[test]
    fn special_characters_pass_through() {
        let cues = vec![Cue::new(0, 1_000, "<i>italic</i> & \"quoted\"")];
        let out = render(&cues);
        assert!(out.contains("<i>italic</i> & \"quoted\""));
    }
}
