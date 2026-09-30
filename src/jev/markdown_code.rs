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
//!   unclosed fence to the end of the document, but an unclosed fence would
//!   then hide every citation after it. Leaving it as text falls back to the
//!   pre-#2003 behaviour, which is the safe direction. A fence may be indented
//!   (a list item's) or quoted (`> `), which is why indentation is not
//!   limited: an indented *code block* is not recognised either way, and a
//!   fence-looking line inside one only matters if a matching closer follows.
//! - **Inline spans** must open and close within one paragraph, so a stray
//!   backtick cannot pair with one paragraphs later and mask what lies
//!   between. A fenced block ends the paragraph it interrupts, and a
//!   backslash-escaped backtick cannot open a span.
//! - **Indented code blocks** are not recognised: without a full parser they
//!   cannot be told apart from list continuation.
//!
//! Callers with several bodies (an issue and its comments) must mask each one
//! on its own, so an unclosed fence or a stray backtick in one cannot pair
//! with markup in another.

use std::ops::Range;

/// What a code character is replaced with.
///
/// Neither whitespace nor a word character, so masking a span can neither
/// glue the tokens around it together (`PR`, a span, `#5`) nor make a number
/// before it run into a word character (`#12` then a span).
const MASK_CHAR: char = '\u{FFFC}';

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

/// Reads `line` as a fence line: any indentation and blockquote markers
/// (`> `), then a run of at least [`MIN_FENCE_LEN`] identical backticks or
/// tildes. Returns the fence and what follows the run.
fn parse_fence_line(line: &str) -> Option<(Fence, &str)> {
    let mut rest = line.trim_end_matches(['\n', '\r']);
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        match rest.strip_prefix('>') {
            Some(after) => rest = after,
            None => break,
        }
    }
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

/// The byte range of every paragraph in `text`: a maximal run of lines that
/// are neither blank nor (already masked) fenced code, so a fenced block ends
/// the paragraph it interrupts.
fn paragraphs(text: &str) -> Vec<Range<usize>> {
    let mut paragraphs = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let separator = trimmed.is_empty() || trimmed.starts_with(MASK_CHAR);
        match (separator, start) {
            (false, None) => start = Some(offset),
            (true, Some(s)) => {
                paragraphs.push(s..offset);
                start = None;
            }
            _ => {}
        }
        offset += line.len();
    }
    if let Some(s) = start {
        paragraphs.push(s..text.len());
    }
    paragraphs
}

/// A maximal run of backticks.
struct Run {
    range: Range<usize>,
    /// Whether a backslash escapes the run's first backtick. That backtick
    /// is then literal text, so the run cannot open a span from it — but
    /// inside a span a backslash is literal, so the run still closes one at
    /// its full length.
    escaped: bool,
}

impl Run {
    /// The length of the run when it is an opener.
    fn opening_len(&self) -> usize {
        self.range.len() - usize::from(self.escaped)
    }
}

/// The byte ranges of every inline code span: a run of backticks closed by
/// the next run of the same length, within one paragraph. An unmatched run is
/// literal.
fn inline_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    for paragraph in paragraphs(text) {
        let runs = backtick_runs(text, paragraph);
        let mut k = 0;
        while k < runs.len() {
            let opener = &runs[k];
            let closer = (opener.opening_len() > 0)
                .then(|| {
                    runs[k + 1..]
                        .iter()
                        .position(|run| run.range.len() == opener.opening_len())
                })
                .flatten();
            match closer {
                Some(offset) => {
                    let start = opener.range.end - opener.opening_len();
                    ranges.push(start..runs[k + 1 + offset].range.end);
                    k += offset + 2;
                }
                None => k += 1,
            }
        }
    }
    ranges
}

/// Every maximal run of backticks within `paragraph`.
fn backtick_runs(text: &str, paragraph: Range<usize>) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    let mut push = |start: usize, end: usize| {
        let backslashes = text[..start]
            .chars()
            .rev()
            .take_while(|&c| c == '\\')
            .count();
        runs.push(Run {
            range: start..end,
            escaped: backslashes % 2 == 1,
        });
    };
    for (i, c) in text[paragraph.clone()].char_indices() {
        let i = paragraph.start + i;
        match (c == '`', start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                push(s, i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        push(s, paragraph.end);
    }
    runs
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

    /// A fence nested in a list item is indented past three spaces.
    #[test]
    fn a_fence_nested_in_a_list_item_is_masked() {
        assert_eq!(
            visible("1. step\n\n       ~~~\n       o/r#1\n       ~~~\n\n#2"),
            "1. step\n\n__________\n____________\n__________\n\n#2"
        );
    }

    #[test]
    fn a_blockquoted_fence_is_masked() {
        assert_eq!(
            visible("> ~~~\n> o/r#1\n> ~~~\n#2"),
            "_____\n_______\n_____\n#2"
        );
    }

    /// A fenced block ends the paragraph it interrupts, so backticks either
    /// side of it cannot pair across it and hide the text between.
    #[test]
    fn a_span_does_not_pair_across_a_fenced_block() {
        assert_eq!(
            visible("a ` b\n```\ncode\n```\nreal #1 ` d"),
            "a ` b\n___\n____\n___\nreal #1 ` d"
        );
    }

    /// An escaped backtick is literal text, not a span delimiter.
    #[test]
    fn an_escaped_backtick_does_not_open_a_span() {
        let text = "use \\`#5\\` literally, see #6";
        assert_eq!(mask_code(text), text);
    }

    #[test]
    fn an_escaped_backtick_does_not_stop_a_later_span() {
        assert_eq!(visible("a \\` b `c` d"), "a \\` b ___ d");
    }

    /// Inside a span a backslash is literal, so `\`` still closes it.
    #[test]
    fn a_backslash_before_a_closing_backtick_still_closes_the_span() {
        assert_eq!(visible("`a\\` #1"), "____ #1");
    }

    /// Masking one body at a time is the caller's job: an unclosed fence in
    /// one pairs with a fence in the next when they are masked together.
    #[test]
    fn bodies_masked_together_can_pair_a_stray_fence_with_a_later_block() {
        assert_ne!(
            mask_code("```\nstray\n\n```\nquoted o/r#9\n```"),
            format!(
                "{}{}",
                mask_code("```\nstray\n"),
                mask_code("\n```\nquoted o/r#9\n```")
            )
        );
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
