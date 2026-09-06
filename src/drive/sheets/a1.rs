//! A1-notation helpers for the Sheets API (issue #1589).
//!
//! Pure — no network, no `SheetsClient` — mirroring `write_gate.rs`'s and
//! `visibility.rs`'s contract, so the fiddly quoting rules are unit-testable
//! on their own.
//!
//! **This module deliberately does not validate A1 grammar.** It does not
//! check column letters or row bounds, resolve defined names, reject
//! unbounded forms, canonicalise case, or verify that a sheet exists. The
//! server is authoritative and already returns a clear error, exactly the
//! stance `crate::cli::drive::read::resolve_export_mime_type` documents for
//! export MIME types ("never validated against `exportLinks` client-side").
//! Rejecting a legal range client-side is a much worse failure than
//! forwarding an illegal one: `Sheet1!A:A`, `Sheet1!1:2`, `Sheet1!A5:A`, a
//! bare `Sheet1`, and a bare defined name like `MyNamedRange` are all valid
//! and all would fall foul of a naive `LETTERS DIGITS : LETTERS DIGITS`
//! validator.
//!
//! What it *does* own is quoting, which is not optional: a title containing
//! a space, an apostrophe or a `!` produces a silently wrong range if pasted
//! in raw, and a sheet literally titled `A1` is ambiguous unless quoted.

use anyhow::Result;

/// Quotes a sheet title for use as an A1 prefix.
///
/// **Always** quotes, even when the title looks safe. Google accepts
/// `'Sheet1'!A1` as readily as `Sheet1!A1`, and always-quoting is the only
/// rule that disambiguates a sheet literally titled `A1` (bare `A1!A1` reads
/// as "cell A1 of the sheet named A1" only by luck of precedence) without
/// this module having to reimplement Google's notion of a "safe" title.
///
/// Inner apostrophes double, per A1 notation: `Bob's Sheet` →
/// `'Bob''s Sheet'`.
pub(crate) fn quote_sheet_title(title: &str) -> String {
    format!("'{}'", title.replace('\'', "''"))
}

/// Splits a leading `Sheet!` prefix off `range`, returning the *unquoted*
/// title and the remainder.
///
/// Returns `None` when `range` carries no sheet prefix at all (a bare
/// `A1:B2`, or a defined name).
///
/// Scans for the separator **outside** the quotes rather than taking the
/// first `!`: a sheet may legitimately be titled `Q1!`, whose prefix is
/// `'Q1!'!` — splitting on the first `!` would yield the nonsense title
/// `'Q1` and lose the rest.
pub(crate) fn split_sheet_prefix(range: &str) -> Option<(String, &str)> {
    if let Some(rest) = range.strip_prefix('\'') {
        // Quoted title: find the closing quote, treating `''` as an escaped
        // apostrophe rather than a terminator.
        let mut title = String::new();
        let mut chars = rest.char_indices();
        while let Some((idx, ch)) = chars.next() {
            if ch != '\'' {
                title.push(ch);
                continue;
            }
            // A doubled quote is a literal apostrophe; anything else closes.
            if rest[idx + 1..].starts_with('\'') {
                title.push('\'');
                chars.next();
                continue;
            }
            let after = &rest[idx + 1..];
            return after.strip_prefix('!').map(|tail| (title, tail));
        }
        // Unterminated quote — not a prefix we can make sense of. Hand it to
        // the server rather than guessing.
        None
    } else {
        let (title, tail) = range.split_once('!')?;
        Some((title.to_string(), tail))
    }
}

/// The sheet title a range refers to, if it names one.
///
/// Used to group `values.batchGet` results back onto the sheets they came
/// from. The API echoes a server-normalised `range` on every `ValueRange`
/// (`Sheet1!A1:Z1000`), which is what makes matching on the response safe
/// where matching on request order is not.
pub(crate) fn sheet_title_of(range: &str) -> Option<String> {
    split_sheet_prefix(range).map(|(title, _)| title)
}

/// Whether `range`, taken as a whole, is itself a quoted reference to an
/// entire other sheet (`'Q1'`, with nothing after the closing quote) —
/// [`quote_sheet_title`]'s own output shape for "the whole sheet named X"
/// (see [`compose`]'s `(Some(sheet), None)` arm).
///
/// [`split_sheet_prefix`] correctly returns `None` for this shape (there is
/// no `!` to split on), but it still names a sheet, so it still conflicts
/// with an explicit `--sheet` the same way `Sheet1!A1:B2` does. An unquoted
/// bare name (`Q1`) is deliberately **not** covered — this module doesn't
/// validate A1 grammar, and a bare identifier is indistinguishable from a
/// defined name.
fn is_whole_sheet_reference(range: &str) -> bool {
    let Some(rest) = range.strip_prefix('\'') else {
        return false;
    };
    let mut chars = rest.char_indices();
    while let Some((idx, ch)) = chars.next() {
        if ch != '\'' {
            continue;
        }
        // A doubled quote is a literal apostrophe, not the closing one.
        if rest[idx + 1..].starts_with('\'') {
            chars.next();
            continue;
        }
        return rest[idx + 1..].is_empty();
    }
    false
}

/// Builds the range to send, from an optional `--sheet` and an optional
/// `--range`.
///
/// - both, `--range` bare ⇒ `'Sheet'!A1:B2`
/// - `--sheet` only ⇒ `'Sheet'` (the whole sheet)
/// - `--range` only ⇒ passed through untouched, prefix or not
/// - neither ⇒ `Err`
///
/// Supplying `--sheet` alongside a `--range` that *already* carries its own
/// prefix is an error rather than a silent precedence rule: the two can
/// disagree, and guessing which the caller meant would write to the wrong
/// sheet.
pub(crate) fn compose(sheet: Option<&str>, range: Option<&str>) -> Result<String> {
    let range = range.map(str::trim).filter(|r| !r.is_empty());
    let sheet = sheet.filter(|s| !s.is_empty());

    match (sheet, range) {
        (Some(sheet), Some(range)) => {
            anyhow::ensure!(
                split_sheet_prefix(range).is_none() && !is_whole_sheet_reference(range),
                "--range {range:?} already names a sheet, so --sheet {sheet:?} would be \
                 ambiguous; drop one of them"
            );
            validate_range(range)?;
            Ok(format!("{}!{}", quote_sheet_title(sheet), range))
        }
        (Some(sheet), None) => Ok(quote_sheet_title(sheet)),
        (None, Some(range)) => {
            validate_range(range)?;
            Ok(range.to_string())
        }
        (None, None) => anyhow::bail!("a range is required: pass --range, --sheet, or both"),
    }
}

/// Rejects the two things that are never a range, whatever Sheets' grammar
/// turns out to accept: an empty string, and one carrying a CR or LF.
///
/// The line-break check mirrors `files_api.rs`'s `validate_content_type`: a
/// newline smuggled into a value that ends up in a URL is a request-splitting
/// shape, not a domain error. Everything else is the server's call — see the
/// module docs.
fn validate_range(range: &str) -> Result<()> {
    anyhow::ensure!(!range.trim().is_empty(), "range must not be empty");
    anyhow::ensure!(
        !range.contains(['\r', '\n']),
        "refusing range {range:?}: must not contain a CR or LF byte"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── quote_sheet_title ──────────────────────────────────────────────

    #[test]
    fn quote_sheet_title_always_quotes_even_a_simple_title() {
        assert_eq!(quote_sheet_title("Sheet1"), "'Sheet1'");
    }

    #[test]
    fn quote_sheet_title_doubles_inner_apostrophes() {
        assert_eq!(quote_sheet_title("Bob's Sheet"), "'Bob''s Sheet'");
        assert_eq!(quote_sheet_title("''"), "''''''");
    }

    #[test]
    fn quote_sheet_title_handles_a_title_containing_a_bang() {
        assert_eq!(quote_sheet_title("Q1!"), "'Q1!'");
    }

    #[test]
    fn quote_sheet_title_disambiguates_a_title_that_looks_like_a_reference() {
        // The whole reason for always-quoting: bare `A1!A1` is ambiguous.
        assert_eq!(quote_sheet_title("A1"), "'A1'");
    }

    // ── split_sheet_prefix ─────────────────────────────────────────────

    #[test]
    fn split_sheet_prefix_none_for_a_bare_range() {
        assert_eq!(split_sheet_prefix("A1:B2"), None);
        assert_eq!(split_sheet_prefix("MyNamedRange"), None);
    }

    #[test]
    fn split_sheet_prefix_splits_an_unquoted_prefix() {
        let (title, rest) = split_sheet_prefix("Sheet1!A1:B2").unwrap();
        assert_eq!(title, "Sheet1");
        assert_eq!(rest, "A1:B2");
    }

    #[test]
    fn split_sheet_prefix_splits_a_quoted_prefix_and_unquotes_it() {
        let (title, rest) = split_sheet_prefix("'My Sheet'!A1:B2").unwrap();
        assert_eq!(title, "My Sheet");
        assert_eq!(rest, "A1:B2");
    }

    #[test]
    fn split_sheet_prefix_unescapes_doubled_apostrophes() {
        let (title, rest) = split_sheet_prefix("'Bob''s Sheet'!A1").unwrap();
        assert_eq!(title, "Bob's Sheet");
        assert_eq!(rest, "A1");
    }

    #[test]
    fn split_sheet_prefix_finds_the_closing_quote_not_the_first_bang() {
        // Splitting on the first `!` would yield the title `'Q1`.
        let (title, rest) = split_sheet_prefix("'Q1!'!A1:B2").unwrap();
        assert_eq!(title, "Q1!");
        assert_eq!(rest, "A1:B2");
    }

    #[test]
    fn split_sheet_prefix_none_for_a_quoted_title_with_no_range() {
        // `'My Sheet'` names a whole sheet; there is no `!` to split on.
        assert_eq!(split_sheet_prefix("'My Sheet'"), None);
    }

    #[test]
    fn split_sheet_prefix_none_for_an_unterminated_quote() {
        assert_eq!(split_sheet_prefix("'unterminated"), None);
    }

    #[test]
    fn split_sheet_prefix_round_trips_through_quote_sheet_title() {
        for title in ["Sheet1", "My Sheet", "Bob's Sheet", "Q1!", "A1", "''"] {
            let composed = format!("{}!A1", quote_sheet_title(title));
            let (parsed, rest) = split_sheet_prefix(&composed).unwrap();
            assert_eq!(parsed, title, "round-trip failed for {title:?}");
            assert_eq!(rest, "A1");
        }
    }

    // ── sheet_title_of ─────────────────────────────────────────────────

    #[test]
    fn sheet_title_of_reads_a_server_normalised_range() {
        assert_eq!(
            sheet_title_of("'My Sheet'!A1:Z1000").as_deref(),
            Some("My Sheet")
        );
        assert_eq!(sheet_title_of("A1:B2"), None);
    }

    // ── compose ────────────────────────────────────────────────────────

    #[test]
    fn compose_joins_sheet_and_bare_range() {
        assert_eq!(
            compose(Some("My Sheet"), Some("A1:B2")).unwrap(),
            "'My Sheet'!A1:B2"
        );
    }

    #[test]
    fn compose_sheet_only_is_the_whole_sheet() {
        assert_eq!(compose(Some("My Sheet"), None).unwrap(), "'My Sheet'");
    }

    #[test]
    fn compose_range_only_passes_through_untouched() {
        assert_eq!(compose(None, Some("A1:B2")).unwrap(), "A1:B2");
        assert_eq!(
            compose(None, Some("'My Sheet'!A1:B2")).unwrap(),
            "'My Sheet'!A1:B2"
        );
    }

    #[test]
    fn compose_preserves_unbounded_forms() {
        // A strict A1 validator would reject every one of these.
        for range in ["A:A", "1:2", "A5:A", "Sheet1"] {
            assert_eq!(compose(None, Some(range)).unwrap(), range);
        }
    }

    #[test]
    fn compose_rejects_sheet_alongside_an_already_prefixed_range() {
        let err = compose(Some("Other"), Some("Sheet1!A1:B2")).unwrap_err();
        assert!(err.to_string().contains("already names a sheet"), "{err}");
    }

    #[test]
    fn compose_rejects_sheet_alongside_a_quoted_whole_sheet_range() {
        // `'Q1'` on its own names a whole other sheet just as much as
        // `Sheet1!A1:B2` does — `split_sheet_prefix` alone would miss this
        // since there's no `!` to split on.
        let err = compose(Some("Other"), Some("'Q1'")).unwrap_err();
        assert!(err.to_string().contains("already names a sheet"), "{err}");
    }

    #[test]
    fn compose_rejects_neither_argument() {
        let err = compose(None, None).unwrap_err();
        assert!(err.to_string().contains("a range is required"), "{err}");
    }

    #[test]
    fn compose_treats_an_empty_or_whitespace_range_as_absent() {
        assert_eq!(compose(Some("S"), Some("   ")).unwrap(), "'S'");
        assert!(compose(None, Some("")).is_err());
    }

    #[test]
    fn compose_trims_surrounding_whitespace_from_a_range() {
        assert_eq!(compose(Some("S"), Some("  A1:B2 ")).unwrap(), "'S'!A1:B2");
    }

    // ── validate_range ─────────────────────────────────────────────────

    #[test]
    fn validate_range_rejects_line_breaks() {
        for bad in ["A1\nB2", "A1\r\nB2", "A1\r"] {
            let err = validate_range(bad).unwrap_err();
            assert!(err.to_string().contains("CR or LF"), "{err}");
        }
        assert!(compose(None, Some("A1\nB2")).is_err());
    }

    #[test]
    fn validate_range_accepts_anything_else_the_server_will_judge() {
        for ok in [
            "A1",
            "A1:B2",
            "A:A",
            "MyNamedRange",
            "!!!",
            "definitely bogus",
        ] {
            validate_range(ok).unwrap();
        }
    }
}
