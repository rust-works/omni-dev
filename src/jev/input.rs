//! Shared input-text truncation for Jev calls (#1779).
//!
//! Both `route` (an issue's text) and `verify-decision` (a cited source's
//! text) cap the text they send to Jev the same way: keep the first and last
//! halves, with a marker in between, so the cut is visible to Jev rather
//! than silent. The tail matters for `route` because a decision comment
//! lands late; for `verify-decision`'s source text the same policy is kept
//! for consistency, though no source has been observed to need it.

/// Marks where an input over the cap was cut.
pub const TRUNCATION_MARKER: &str = "[... truncated]";

/// Truncates `text` to `max_chars`, keeping its first and last halves with
/// [`TRUNCATION_MARKER`] between them.
///
/// Returns the (possibly unchanged) text and whether it was truncated. Cuts
/// on a `char` boundary, never inside a multi-byte UTF-8 sequence.
#[must_use]
pub fn truncate_middle(text: &str, max_chars: usize) -> (String, bool) {
    if text.char_indices().nth(max_chars).is_none() {
        return (text.to_string(), false);
    }
    let total = text.chars().count();
    let head = max_chars / 2;
    let tail = max_chars - head;
    let byte_at = |chars: usize| {
        text.char_indices()
            .nth(chars)
            .map_or(text.len(), |(i, _)| i)
    };
    let (head_end, tail_start) = (byte_at(head), byte_at(total - tail));
    let cut = format!(
        "{}\n\n{TRUNCATION_MARKER}\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    );
    (cut, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_unchanged() {
        let (out, truncated) = truncate_middle("short", 1000);
        assert_eq!(out, "short");
        assert!(!truncated);
    }

    #[test]
    fn truncates_on_a_char_boundary() {
        let text = "é".repeat(100);
        let (full, _) = truncate_middle(&text, usize::MAX);
        let (out, truncated) = truncate_middle(&text, 20);
        assert!(truncated);
        let (head, tail) = out
            .split_once(&format!("\n\n{TRUNCATION_MARKER}\n\n"))
            .unwrap();
        assert_eq!(head.chars().count(), 10);
        assert_eq!(tail.chars().count(), 10);
        assert!(full.starts_with(head));
        assert!(full.ends_with(tail));
    }

    #[test]
    fn handles_a_one_char_cap() {
        // head = 0, tail = 1: no head text, just the very last character.
        let (out, truncated) = truncate_middle("hello world", 1);
        assert!(truncated);
        assert_eq!(out, format!("\n\n{TRUNCATION_MARKER}\n\nd"));
    }

    #[test]
    fn exactly_at_the_cap_is_not_truncated() {
        let (out, truncated) = truncate_middle("hello", 5);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }
}
