//! WebVTT (`.vtt`) renderer.
//!
//! ```text
//! WEBVTT
//!
//! 00:00:01.000 --> 00:00:04.000
//! Subtitle text
//!
//! ...
//! ```

use std::fmt::Write;

use crate::transcript::cue::Cue;

/// Render `cues` to a WebVTT-formatted string. Always emits the `WEBVTT`
/// header even for an empty cue list, so the output is a valid WebVTT
/// document.
pub fn render(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
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

/// Format `ms` as `HH:MM:SS.mmm` (WebVTT's period decimal separator).
fn format_timestamp(ms: u64) -> String {
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    let millis = ms % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_emits_header_only() {
        assert_eq!(render(&[]), "WEBVTT\n\n");
    }

    #[test]
    fn header_precedes_cues() {
        let out = render(&[Cue::new(0, 1_000, "x")]);
        assert!(out.starts_with("WEBVTT\n\n"));
    }

    #[test]
    fn single_cue_basic() {
        let cues = vec![Cue::new(1_000, 4_000, "Hello, world.")];
        let out = render(&cues);
        assert_eq!(
            out,
            "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nHello, world.\n\n"
        );
    }

    #[test]
    fn cues_are_not_numbered() {
        let cues = vec![Cue::new(0, 1_000, "one"), Cue::new(1_000, 2_000, "two")];
        let out = render(&cues);
        // No leading "1\n" before the timecode (unlike SRT).
        assert!(!out.contains("\n1\n00:"));
        assert!(!out.contains("\n2\n00:"));
    }

    #[test]
    fn timestamps_use_period_separator_not_comma() {
        let ts = format_timestamp(1_500);
        assert!(ts.contains('.'));
        assert!(!ts.contains(','));
    }

    #[test]
    fn timestamps_pad_zero() {
        assert_eq!(format_timestamp(0), "00:00:00.000");
    }

    #[test]
    fn timestamps_handle_subsecond_millis() {
        assert_eq!(format_timestamp(1), "00:00:00.001");
        assert_eq!(format_timestamp(999), "00:00:00.999");
    }

    #[test]
    fn timestamps_handle_hour_boundary() {
        assert_eq!(format_timestamp(3_600_000), "01:00:00.000");
        assert_eq!(format_timestamp(3_599_999), "00:59:59.999");
    }

    #[test]
    fn timestamps_handle_multi_hour() {
        assert_eq!(
            format_timestamp(2 * 3_600_000 + 30 * 60_000 + 45 * 1_000 + 678),
            "02:30:45.678"
        );
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
        assert!(out.contains("00:00:00.500 --> 00:00:00.500"));
        assert!(out.contains("instant"));
    }

    #[test]
    fn empty_text_cue_yields_blank_content_line() {
        let cues = vec![Cue::new(0, 1_000, "")];
        let out = render(&cues);
        assert_eq!(out, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n\n\n");
    }

    #[test]
    fn output_ends_with_blank_line_separator() {
        let cues = vec![Cue::new(0, 1_000, "x")];
        let out = render(&cues);
        assert!(out.ends_with("\n\n"));
    }
}
