//! Plain-text renderer — cue text only, one cue per line, no timing.

use crate::transcript::cue::Cue;

/// Render `cues` as plain text: each cue's `text` followed by a newline,
/// concatenated. Cues with embedded newlines retain them. Empty input
/// yields `""`.
pub fn render(cues: &[Cue]) -> String {
    let mut out = String::new();
    for cue in cues {
        out.push_str(&cue.text);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(render(&[]), "");
    }

    #[test]
    fn single_cue() {
        let out = render(&[Cue::new(0, 1_000, "hello")]);
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn multiple_cues_joined_by_newline() {
        let cues = vec![
            Cue::new(0, 1_000, "one"),
            Cue::new(1_000, 2_000, "two"),
            Cue::new(2_000, 3_000, "three"),
        ];
        assert_eq!(render(&cues), "one\ntwo\nthree\n");
    }

    #[test]
    fn embedded_newlines_preserved() {
        let cues = vec![Cue::new(0, 1_000, "line one\nline two")];
        assert_eq!(render(&cues), "line one\nline two\n");
    }

    #[test]
    fn timing_is_omitted() {
        let cues = vec![Cue::new(123_456, 789_012, "text")];
        let out = render(&cues);
        assert!(!out.contains("123"));
        assert!(!out.contains("789"));
        assert!(!out.contains(':'));
        assert_eq!(out, "text\n");
    }

    #[test]
    fn empty_cue_text_yields_blank_line() {
        let cues = vec![Cue::new(0, 1_000, ""), Cue::new(1_000, 2_000, "x")];
        assert_eq!(render(&cues), "\nx\n");
    }

    #[test]
    fn output_always_ends_with_newline_when_nonempty() {
        let cues = vec![Cue::new(0, 1_000, "hello")];
        assert!(render(&cues).ends_with('\n'));
    }
}
