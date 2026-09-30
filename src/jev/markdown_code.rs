//! Masks the code in a piece of Markdown, so text scanners can skip it (#2003).
//!
//! A quoted error message or example inside a fenced code block or an inline
//! code span is not prose: `owner/repo#123` in one is an example, not a
//! reference. [`mask_code`] replaces every code character with
//! `MASK_CHAR`, leaving everything else (including every newline) as it
//! was, so an existing regex can run on the result unchanged.
//!
//! This is deliberately a small subset of CommonMark, not a parser:
//!
//! - **Fenced blocks** are recognised only when *closed*. CommonMark runs an
//!   unclosed fence to the end of the document, but `route` scans one long
//!   state built from an issue's body and every comment, so an unclosed fence
//!   in one body would hide every later citation. Leaving it as text falls
//!   back to the pre-#2003 behaviour, which is the safe direction.
//! - **Inline spans** must open and close within one paragraph, so a stray
//!   backtick cannot pair with one paragraphs later and mask what lies
//!   between.
//! - **Indented code blocks** are not recognised: without a full parser they
//!   cannot be told apart from list continuation.

use std::ops::Range;

/// What a code character is replaced with.
///
/// Neither whitespace nor a word character, so masking a span can neither
/// glue the tokens around it together (`PR`, a span, `#5`) nor make a number
/// before it run into a word character (`#12` then a span).
const MASK_CHAR: char = '\u{FFFC}';

/// The most leading spaces a fence line may carry before it is an indented
/// code block instead.
const MAX_FENCE_INDENT: usize = 3;

/// The shortest run of fence characters that opens or closes a fence.
const MIN_FENCE_LEN: usize = 3;

/// Returns `text` with fenced code blocks and inline code spans replaced by
/// `MASK_CHAR`, newlines preserved.
#[must_use]
pub fn mask_code(text: &str) -> String {
    let unfenced = apply_mask(text, &fenced_ranges(text));
    apply_mask(&unfenced, &inline_ranges(&unfenced))
}

/// Replaces every non-newline character inside `ranges` (sorted and
/// non-overlapping) with `MASK_CHAR`.
fn apply_mask(text: &str, ranges: &[Range<usize>]) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in ranges {
        out.push_str(&text[cursor..range.start]);
        out.extend(
            text[range.clone()]
                .chars()
                .map(|c| if c == '\n' { c } else { MASK_CHAR }),
        );
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// A fence's marker character and the length of its opening run.
#[derive(Clone, Copy)]
struct Fence {
    marker: char,
    len: usize,
}

/// Reads `line` as a fence line: up to [`MAX_FENCE_INDENT`] spaces, then a
/// run of at least [`MIN_FENCE_LEN`] identical backticks or tildes. Returns
/// the fence and what follows the run.
fn parse_fence_line(line: &str) -> Option<(Fence, &str)> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let indent = trimmed.len() - trimmed.trim_start_matches(' ').len();
    if indent > MAX_FENCE_INDENT {
        return None;
    }
    let rest = &trimmed[indent..];
    let marker = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = rest.chars().take_while(|&c| c == marker).count();
    (len >= MIN_FENCE_LEN).then(|| {
        (
            Fence { marker, len },
            &rest[len..], // the marker is one byte, so `len` chars is `len` bytes
        )
    })
}

/// Whether `line` opens a fence, returning it. A backtick fence's info
/// string may not contain a backtick, or the line is an inline span.
fn opening_fence(line: &str) -> Option<Fence> {
    let (fence, info) = parse_fence_line(line)?;
    (fence.marker != '`' || !info.contains('`')).then_some(fence)
}

/// Whether `line` closes `open`: the same marker, at least as long, and
/// nothing after it but whitespace.
fn closes_fence(line: &str, open: Fence) -> bool {
    parse_fence_line(line).is_some_and(|(fence, after)| {
        fence.marker == open.marker && fence.len >= open.len && after.trim().is_empty()
    })
}

/// The byte range of every *closed* fenced block, opening line through
/// closing line.
fn fenced_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut open: Option<(Fence, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        match open {
            None => {
                if let Some(fence) = opening_fence(line) {
                    open = Some((fence, offset));
                }
            }
            Some((fence, start)) => {
                if closes_fence(line, fence) {
                    ranges.push(start..end);
                    open = None;
                }
            }
        }
        offset = end;
    }
    ranges
}

/// The byte ranges of every inline code span: a run of backticks closed by
/// the next run of the same length, within one paragraph. An unmatched run is
/// literal.
fn inline_ranges(text: &str) -> Vec<Range<usize>> {
    let runs = backtick_runs(text);
    let mut ranges = Vec::new();
    let mut k = 0;
    while k < runs.len() {
        let opener = &runs[k];
        let closer = runs[k + 1..]
            .iter()
            .take_while(|run| !has_blank_line(&text[opener.end..run.start]))
            .position(|run| run.len() == opener.len());
        match closer {
            Some(offset) => {
                let closer = &runs[k + 1 + offset];
                ranges.push(opener.start..closer.end);
                k += offset + 2;
            }
            None => k += 1,
        }
    }
    ranges
}

/// The byte range of every maximal run of backticks in `text`.
fn backtick_runs(text: &str) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (c == '`', start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push(s..text.len());
    }
    runs
}

/// Whether `segment` contains a blank line, which ends a paragraph. The
/// first and last pieces are the partial lines either side of the segment,
/// so only the ones between them can be blank.
fn has_blank_line(segment: &str) -> bool {
    let pieces: Vec<&str> = segment.split('\n').collect();
    pieces.len() > 2
        && pieces[1..pieces.len() - 1]
            .iter()
            .any(|p| p.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Masks `text` and renders each masked character as `_`, so an
    /// expectation reads as the visible remainder.
    fn visible(text: &str) -> String {
        mask_code(text)
            .chars()
            .map(|c| if c == MASK_CHAR { '_' } else { c })
            .collect()
    }

    #[test]
    fn text_without_code_is_unchanged() {
        let text = "See #1614 and owner/repo#5.\n\nA second paragraph.";
        assert_eq!(mask_code(text), text);
    }

    #[test]
    fn a_backtick_fence_is_masked_through_its_closing_line() {
        assert_eq!(
            visible("before\n```\no/r#1\n```\nafter #2"),
            "before\n___\n_____\n___\nafter #2"
        );
    }

    #[test]
    fn a_tilde_fence_is_masked() {
        assert_eq!(visible("~~~\no/r#1\n~~~\n#2"), "___\n_____\n___\n#2");
    }

    #[test]
    fn a_fence_info_string_is_part_of_the_block() {
        assert_eq!(
            visible("```rust\no/r#1\n```\n#2"),
            "_______\n_____\n___\n#2"
        );
    }

    #[test]
    fn a_closing_fence_may_be_longer_than_the_opening_one() {
        assert_eq!(visible("```\no/r#1\n`````\n#2"), "___\n_____\n_____\n#2");
    }

    #[test]
    fn a_shorter_run_does_not_close_a_fence() {
        // The four-backtick fence stays open past the three-backtick line and
        // is closed by the last line.
        assert_eq!(
            visible("````\n```\no/r#1\n````\n#2"),
            "____\n___\n_____\n____\n#2"
        );
    }

    #[test]
    fn a_fence_of_the_other_marker_does_not_close_a_fence() {
        assert_eq!(
            visible("```\n~~~\no/r#1\n```\n#2"),
            "___\n___\n_____\n___\n#2"
        );
    }

    #[test]
    fn a_closing_fence_carrying_text_does_not_close_the_fence() {
        assert_eq!(
            visible("```\n``` x\no/r#1\n```\n#2"),
            "___\n_____\n_____\n___\n#2"
        );
    }

    #[test]
    fn a_fence_indented_up_to_three_spaces_is_still_a_fence() {
        assert_eq!(
            visible("   ```\no/r#1\n   ```\n#2"),
            "______\n_____\n______\n#2"
        );
    }

    #[test]
    fn a_line_indented_four_spaces_is_not_a_fence() {
        // Were the first line a fence, the longer run would close it. As it
        // is not, the two runs differ in length and pair as nothing.
        let text = "    ```\no/r#1\n````\n#2";
        assert_eq!(mask_code(text), text);
    }

    #[test]
    fn an_unclosed_fence_is_left_as_text() {
        let text = "```\no/r#1\nstill going #2";
        assert_eq!(mask_code(text), text);
    }

    #[test]
    fn a_backtick_line_whose_info_string_has_a_backtick_is_not_a_fence() {
        // Inline code, not a fence: only the span is masked.
        assert_eq!(visible("``` a`b` c\n#2"), "``` a___ c\n#2");
    }

    #[test]
    fn a_crlf_fence_is_masked() {
        assert_eq!(
            visible("```\r\no/r#1\r\n```\r\n#2"),
            "____\n______\n____\n#2"
        );
    }

    #[test]
    fn an_inline_span_is_masked_and_the_rest_is_kept() {
        assert_eq!(visible("use `o/r#1` here #2"), "use _______ here #2");
    }

    #[test]
    fn a_double_backtick_span_may_contain_a_single_backtick() {
        assert_eq!(visible("x ``a ` b`` y"), "x _________ y");
    }

    #[test]
    fn an_unmatched_backtick_run_is_literal() {
        let text = "a ` b #1";
        assert_eq!(mask_code(text), text);
    }

    #[test]
    fn runs_of_different_lengths_do_not_pair() {
        let text = "a `` b ` c #1";
        assert_eq!(mask_code(text), text);
    }

    #[test]
    fn an_inline_span_may_cross_a_line_break() {
        assert_eq!(visible("`o/r\n#1` #2"), "____\n___ #2");
    }

    /// A stray backtick must not pair with one in a later paragraph and hide
    /// what is between them.
    #[test]
    fn an_inline_span_does_not_cross_a_blank_line() {
        let text = "a ` b\n\nreal #1\n\nc ` d";
        assert_eq!(mask_code(text), text);
    }

    #[test]
    fn an_unmatched_opener_does_not_stop_a_later_pair_matching() {
        assert_eq!(visible("`a\n\nb `c` d"), "`a\n\nb ___ d");
    }

    #[test]
    fn two_spans_on_one_line_are_masked_separately() {
        assert_eq!(visible("`a` #1 `b`"), "___ #1 ___");
    }

    #[test]
    fn a_span_inside_a_fence_does_not_open_a_second_mask() {
        assert_eq!(visible("```\n`a`\n```\n#1"), "___\n___\n___\n#1");
    }

    #[test]
    fn masking_keeps_multibyte_text_around_a_span_intact() {
        assert_eq!(visible("café `é#1` ünï #2"), "café _____ ünï #2");
    }

    #[test]
    fn every_newline_survives_masking() {
        let text = "a\n```\nb\n\nc\n```\n`d\ne`\nf\n";
        assert_eq!(
            mask_code(text).matches('\n').count(),
            text.matches('\n').count()
        );
    }

    #[test]
    fn an_empty_string_masks_to_an_empty_string() {
        assert_eq!(mask_code(""), "");
    }
}
