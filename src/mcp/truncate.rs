//! Large-output handling for MCP tool responses.
//!
//! MCP clients have to render whatever a tool returns into a chat transcript.
//! Responses in the megabytes can blow out context windows and make the
//! transcript unreadable. We cap responses at a default limit and report that
//! truncation happened so the client/assistant can react (e.g., narrow the
//! range, request pagination).

/// Default maximum response size in bytes (100 KB).
///
/// Chosen as a practical balance: large enough to hold most commit-range
/// analyses, JIRA issue bodies, or Confluence page content, small enough to
/// stay well under common client context limits.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 100 * 1024;

/// Truncation marker appended to responses that exceed the limit.
const TRUNCATION_MARKER: &str = "\n\n[output truncated]";

/// Truncates `text` to at most `limit` bytes, preserving a UTF-8 boundary.
///
/// Returns the (possibly truncated) text and a flag indicating whether
/// truncation happened. When truncated, a small marker is appended so callers
/// reading the raw text still see a clear signal; programmatic callers should
/// rely on the boolean flag instead.
///
/// A `limit` of 0 is treated as "no limit" because it is almost always a
/// configuration mistake rather than an explicit request to drop everything.
pub fn truncate_response(text: String, limit: usize) -> (String, bool) {
    if limit == 0 || text.len() <= limit {
        return (text, false);
    }

    // `floor_char_boundary` is nightly-only, so walk backwards from `limit`
    // until we land on a valid char boundary.
    let mut cutoff = limit;
    while cutoff > 0 && !text.is_char_boundary(cutoff) {
        cutoff -= 1;
    }

    let mut truncated = text;
    truncated.truncate(cutoff);
    truncated.push_str(TRUNCATION_MARKER);
    (truncated, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_unchanged() {
        let (out, truncated) = truncate_response("hello".to_string(), 100);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    fn exact_length_is_unchanged() {
        let (out, truncated) = truncate_response("hello".to_string(), 5);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    fn over_limit_is_truncated_with_marker() {
        let input = "a".repeat(1000);
        let (out, truncated) = truncate_response(input, 100);
        assert!(truncated);
        assert!(out.len() < 1000);
        assert!(out.starts_with(&"a".repeat(100)));
        assert!(out.contains("[output truncated]"));
    }

    #[test]
    fn zero_limit_means_no_limit() {
        let input = "abc".repeat(1000);
        let original_len = input.len();
        let (out, truncated) = truncate_response(input, 0);
        assert!(!truncated);
        assert_eq!(out.len(), original_len);
    }

    #[test]
    fn utf8_boundary_preserved() {
        // Four-byte emoji; cutting in the middle would produce invalid UTF-8.
        // The input is a sequence of 🦀 (4 bytes each); cap mid-codepoint.
        let input: String = "🦀".repeat(50);
        let (out, truncated) = truncate_response(input, 10); // mid-codepoint
        assert!(truncated);
        // Must be valid UTF-8 — `String` enforces this, but the content
        // before the marker must also align to a char boundary so no partial
        // emoji are present.
        let body = out.trim_end_matches("[output truncated]");
        // Every character in the body is the crab emoji.
        for ch in body.chars() {
            assert!(ch == '🦀' || ch == '\n');
        }
    }

    #[test]
    fn default_cap_is_100kb() {
        assert_eq!(DEFAULT_MAX_RESPONSE_BYTES, 102_400);
    }

    #[test]
    fn empty_string_is_not_truncated() {
        let (out, truncated) = truncate_response(String::new(), 100);
        assert_eq!(out, "");
        assert!(!truncated);
    }
}
