//! Renders a raw MIME message (an archived `.eml`, or any RFC 5322 message)
//! as human-readable Markdown, backing `gmail render` and `gmail read -o
//! markdown` (#1513).
//!
//! A sibling of [`crate::gmail::attachments`], not an extension of
//! [`crate::gmail::raw_message`]'s line-oriented scanner: producing readable
//! text needs the same real MIME/multipart parsing `attachments.rs` already
//! pulls in (`mail-parser`), plus RFC 2047 encoded-word decoding of header
//! values — which that scanner's own doc comment explicitly says it does not
//! attempt.

use std::fmt::Write as _;

use mail_parser::{Addr, Address, HeaderValue, Message, MessageParser, MimeHeaders};
#[cfg(test)]
use mail_parser::{ContentType, Encoding, Header, HeaderName, MessagePart, PartType};

use crate::gmail::attachments::extract_attachments;

/// Renders `raw` as Markdown: a header block (Subject/From/To/Cc/Date/
/// Message-Id/In-Reply-To/References, RFC 2047-decoded courtesy of
/// `mail-parser`), the message body (preferring `text/plain`, falling back
/// to `text/html` converted via `htmd`), and a bullet list of attachment
/// filenames (listed, never embedded — this is a readable rendering, not an
/// export).
///
/// `fold_quotes` controls whether deeply-nested `>`-quoted reply history in
/// the body is collapsed into one-line markers (#1514) — see
/// [`fold_quoted_lines`].
///
/// Never fails: an unparseable message degrades to a short placeholder
/// rather than erroring, the same posture
/// [`extract_attachments`] takes so a batch caller like `gmail render`
/// never has one bad file abort the whole run.
pub(crate) fn render_markdown(raw: &[u8], fold_quotes: bool) -> String {
    let Some(message) = MessageParser::default().parse(raw) else {
        return "*(unable to parse this message)*\n".to_string();
    };

    let mut out = String::new();
    let _ = writeln!(out, "# {}\n", message.subject().unwrap_or("(no subject)"));

    write_header(&mut out, "From", format_address(message.from()));
    write_header(&mut out, "To", format_address(message.to()));
    write_header(&mut out, "Cc", format_address(message.cc()));
    write_header(
        &mut out,
        "Date",
        message.date().map(mail_parser::DateTime::to_rfc822),
    );
    write_header(
        &mut out,
        "Message-Id",
        message.message_id().map(str::to_string),
    );
    write_header(
        &mut out,
        "In-Reply-To",
        format_header_value(message.in_reply_to()),
    );
    write_header(
        &mut out,
        "References",
        format_header_value(message.references()),
    );

    out.push('\n');
    out.push_str(body_markdown(&message, fold_quotes).trim_end());
    out.push('\n');

    let attachments = extract_attachments(raw);
    if !attachments.is_empty() {
        out.push_str("\n## Attachments\n\n");
        for attachment in &attachments {
            let _ = writeln!(out, "- {}", attachment.filename);
        }
    }

    out
}

/// Appends a `- **label:** value` bullet when `value` is present. The
/// header block is a bullet list rather than bare lines so every field
/// reliably lands on its own line under CommonMark, where consecutive plain
/// text lines are soft-wrapped into a single paragraph.
fn write_header(out: &mut String, label: &str, value: Option<String>) {
    if let Some(value) = value {
        let _ = writeln!(out, "- **{label}:** {value}");
    }
}

/// Formats an address header's value as a comma-separated `"Name
/// <email>"` list, or `None` when the header is absent/empty.
fn format_address(address: Option<&Address>) -> Option<String> {
    let address = address?;
    let formatted: Vec<String> = address.iter().map(format_addr).collect();
    (!formatted.is_empty()).then(|| formatted.join(", "))
}

fn format_addr(addr: &Addr) -> String {
    match (&addr.name, &addr.address) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (Some(name), None) => name.to_string(),
        (None, Some(email)) => email.to_string(),
        (None, None) => String::new(),
    }
}

/// Formats an `In-Reply-To`/`References`-shaped header value: a single
/// message-id, or a whitespace-separated list of them.
fn format_header_value(value: &HeaderValue) -> Option<String> {
    match value {
        HeaderValue::Text(text) => Some(text.to_string()),
        HeaderValue::TextList(list) if !list.is_empty() => {
            Some(list.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" "))
        }
        _ => None,
    }
}

/// Prefers the message's genuine `text/plain` body; falls back to its
/// `text/html` body converted to Markdown via `htmd` (`mail-parser` resolves
/// MIME structure but does no HTML-to-Markdown rendering itself). A
/// conversion error — real HTML in practice never triggers one, since
/// `htmd`'s underlying `html5ever` parser tolerates malformed markup like a
/// browser would — falls back to the raw HTML text rather than dropping the
/// body entirely.
///
/// Deliberately checks the resolved part's actual content type rather than
/// just calling `body_text(0)` first: for an HTML-only message, `mail-parser`
/// still populates `text_body` by pointing it at that same HTML part and
/// returning its own crude tag-stripped rendering from `body_text` — that
/// would silently take priority over `htmd`'s much better Markdown
/// conversion for the overwhelmingly common HTML-only-marketing-mail case
/// (see #1513) unless genuineness is checked first.
fn body_markdown(message: &Message, fold_quotes: bool) -> String {
    if message
        .text_part(0)
        .is_some_and(|part| part.is_content_type("text", "plain"))
    {
        if let Some(text) = message.body_text(0) {
            return finish_body(text.into_owned(), fold_quotes);
        }
    }
    if let Some(html) = message.body_html(0) {
        let converted = htmd::convert(&html).unwrap_or_else(|_| html.into_owned());
        return finish_body(converted, fold_quotes);
    }
    if let Some(text) = message.body_text(0) {
        return finish_body(text.into_owned(), fold_quotes);
    }
    "*(no body)*".to_string()
}

/// Applies [`fold_quoted_lines`] to a resolved body when requested. Shared
/// by every `body_markdown` return branch (genuine plain-text, `htmd`-
/// converted HTML, and the final plain-text fallback) so folding behaves
/// identically regardless of which branch produced the body.
fn finish_body(body: String, fold_quotes: bool) -> String {
    if fold_quotes {
        fold_quoted_lines(&body)
    } else {
        body
    }
}

/// Depth beyond which a quote block is folded (#1514). The immediately-
/// preceding reply's quote (depth 1) stays visible for context; anything
/// nested deeper is replaced with a one-line marker. A fixed constant
/// rather than a CLI-configurable number — nothing yet motivates per-call
/// tuning, and the originating issue only asked for "some threshold".
const FOLD_QUOTE_DEPTH_THRESHOLD: u32 = 1;

/// Counts a line's leading `>` quote markers, tolerating any amount of
/// whitespace between them — so `>>>`, `> > >`, and mixed spacing like
/// `>>  >` all count as depth 3. A line with no leading `>` has depth 0.
fn quote_depth(line: &str) -> u32 {
    let mut depth = 0;
    let mut rest = line;
    while let Some(next) = rest.trim_start_matches(' ').strip_prefix('>') {
        depth += 1;
        rest = next;
    }
    depth
}

/// Folds `>`-quoted lines nested deeper than [`FOLD_QUOTE_DEPTH_THRESHOLD`]
/// into a one-line `*(N quoted lines omitted)*` marker, leaving shallower
/// quote levels and unquoted content untouched. Operates purely on
/// `>`-prefixed lines, so it applies identically to native plain-text
/// quoting and to `htmd`'s `<blockquote>`-to-`>`-line conversion (both
/// converge on the same per-line `>`-depth shape by the time a body reaches
/// this function — see the module doc comment).
///
/// A run of blank lines sitting between two foldable quote blocks is
/// bridged into the fold rather than left as a visible gap, so a lone blank
/// quote-separator line doesn't fragment one nested quote block into
/// several tiny markers.
fn fold_quoted_lines(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut foldable: Vec<bool> = lines
        .iter()
        .map(|line| quote_depth(line) > FOLD_QUOTE_DEPTH_THRESHOLD)
        .collect();

    let mut i = 0;
    while i < foldable.len() {
        if foldable[i] || !lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let gap_start = i;
        while i < foldable.len() && !foldable[i] && lines[i].trim().is_empty() {
            i += 1;
        }
        let bridged = gap_start > 0 && foldable[gap_start - 1] && i < foldable.len() && foldable[i];
        if bridged {
            for folded in &mut foldable[gap_start..i] {
                *folded = true;
            }
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !foldable[i] {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && foldable[i] {
            i += 1;
        }
        let count = i - start;
        let plural = if count == 1 { "" } else { "s" };
        out.push(format!("*({count} quoted line{plural} omitted)*"));
    }
    out.join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn render_markdown_renders_plain_text_message() {
        let raw =
            b"Subject: Hello\r\nFrom: Alice <a@example.com>\r\nTo: b@example.com\r\n\r\nHi there.";
        let markdown = render_markdown(raw, false);
        assert!(markdown.contains("# Hello"));
        assert!(markdown.contains("- **From:** Alice <a@example.com>"));
        assert!(markdown.contains("- **To:** b@example.com"));
        assert!(markdown.contains("Hi there."));
    }

    #[test]
    fn render_markdown_prefers_text_plain_in_multipart_alternative() {
        let raw = b"Subject: Hi\r\nContent-Type: multipart/alternative; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nPlain body\r\n\
--B\r\nContent-Type: text/html\r\n\r\n<p>HTML body</p>\r\n\
--B--\r\n";
        let markdown = render_markdown(raw, false);
        assert!(markdown.contains("Plain body"));
        assert!(!markdown.contains("HTML body"));
    }

    #[test]
    fn render_markdown_falls_back_to_html_when_no_plain_text_part() {
        let raw = b"Subject: Hi\r\nContent-Type: text/html\r\n\r\n<p>Only <b>HTML</b> here.</p>";
        let markdown = render_markdown(raw, false);
        assert!(markdown.contains("Only **HTML** here."));
    }

    #[test]
    fn render_markdown_decodes_rfc2047_encoded_subject() {
        let raw = b"Subject: =?utf-8?Q?You=20have=20a=20new=20message?=\r\nFrom: a@example.com\r\n\r\nBody.";
        let markdown = render_markdown(raw, false);
        assert!(markdown.contains("# You have a new message"));
    }

    #[test]
    fn render_markdown_lists_attachment_filenames_without_embedding_contents() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\n\r\ndata\r\n\
--B--\r\n";
        let markdown = render_markdown(raw, false);
        assert!(markdown.contains("See attached."));
        assert!(markdown.contains("## Attachments"));
        assert!(markdown.contains("- report.pdf"));
        assert!(!markdown.contains("data\n"));
    }

    #[test]
    fn render_markdown_omits_attachments_section_when_none_present() {
        let raw = b"Subject: Hi\r\nFrom: a@example.com\r\n\r\nJust text.";
        let markdown = render_markdown(raw, false);
        assert!(!markdown.contains("## Attachments"));
    }

    #[test]
    fn render_markdown_degrades_gracefully_on_unparseable_input() {
        let markdown = render_markdown(b"", false);
        assert!(!markdown.is_empty());
    }

    #[test]
    fn render_markdown_omits_absent_optional_headers() {
        let raw = b"Subject: Hi\r\nFrom: a@example.com\r\n\r\nBody.";
        let markdown = render_markdown(raw, false);
        assert!(!markdown.contains("**Cc:**"));
        assert!(!markdown.contains("**In-Reply-To:**"));
        assert!(!markdown.contains("**References:**"));
    }

    #[test]
    fn render_markdown_includes_in_reply_to_and_references_when_present() {
        let raw = b"Subject: Re: Hi\r\nIn-Reply-To: <id1@example.com>\r\nReferences: <id1@example.com> <id2@example.com>\r\n\r\nBody.";
        let markdown = render_markdown(raw, false);
        assert!(markdown.contains("- **In-Reply-To:** id1@example.com"));
        assert!(markdown.contains("- **References:** id1@example.com id2@example.com"));
    }

    #[test]
    fn render_markdown_renders_a_bare_display_name_address_without_an_email() {
        // mail-parser leniently parses a `From:` header with no `<email>`
        // as an address with a name but no address.
        let raw = b"Subject: Hi\r\nFrom: Alice\r\n\r\nBody.";
        let markdown = render_markdown(raw, false);
        assert!(markdown.contains("- **From:** Alice"));
    }

    #[test]
    fn format_addr_renders_empty_string_when_name_and_address_are_both_absent() {
        // Not reachable through mail-parser's own output (it drops
        // malformed address entries rather than yielding one with neither
        // field set) — exercised directly against the pure helper instead.
        assert_eq!(
            format_addr(&Addr {
                name: None,
                address: None
            }),
            ""
        );
    }

    /// Builds a single-part synthetic `Message` whose one part declares
    /// `type_`/`subtype` as its Content-Type header and carries `body` as
    /// its parsed content, with `text_body` (and no `html_body`) pointing
    /// at it. `mail-parser`'s own parser always keeps a part's declared
    /// Content-Type and its parsed [`PartType`] in sync (a `text/plain`
    /// header is always parsed as `PartType::Text`, never
    /// `PartType::Binary`), so the mismatches below aren't reachable through
    /// real parsing — this constructs them directly, the same workaround
    /// `format_addr_renders_empty_string_when_name_and_address_are_both_absent`
    /// above uses for a state `mail-parser` itself never produces.
    fn synthetic_single_part_message(
        type_: &str,
        subtype: &str,
        body: PartType<'static>,
    ) -> Message<'static> {
        let part = MessagePart {
            headers: vec![Header {
                name: HeaderName::ContentType,
                value: HeaderValue::ContentType(ContentType {
                    c_type: type_.to_string().into(),
                    c_subtype: Some(subtype.to_string().into()),
                    attributes: None,
                }),
                offset_field: 0,
                offset_start: 0,
                offset_end: 0,
            }],
            is_encoding_problem: false,
            body,
            encoding: Encoding::None,
            offset_header: 0,
            offset_body: 0,
            offset_end: 0,
        };
        Message {
            html_body: vec![],
            text_body: vec![0],
            attachments: vec![],
            parts: vec![part],
            raw_message: Vec::new().into(),
        }
    }

    #[test]
    fn body_markdown_falls_back_past_a_declared_plain_text_part_with_no_actual_text_body() {
        // text_part(0) reports text/plain, but the part's actual body is
        // PartType::Binary — a state real parsing never produces (see
        // `synthetic_single_part_message`'s doc comment).
        let message = synthetic_single_part_message(
            "text",
            "plain",
            PartType::Binary(b"irrelevant".as_slice().into()),
        );
        assert_eq!(body_markdown(&message, false), "*(no body)*");
    }

    #[test]
    fn body_markdown_falls_back_to_plain_text_body_for_a_non_plain_text_part() {
        // text_part(0) reports text/csv (not text/plain), so the genuine-
        // plain-text branch is skipped; there's no html_body entry, so this
        // falls through to the final body_text(0) fallback.
        let message = synthetic_single_part_message("text", "csv", PartType::Text("a,b,c".into()));
        assert_eq!(body_markdown(&message, false), "a,b,c");
    }

    #[test]
    fn body_markdown_falls_back_to_no_body_placeholder_when_the_only_part_is_an_attachment() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=\"a.txt\"\r\n\r\nAttached text\r\n\
--B--\r\n";
        let raw = raw.to_vec();
        let message = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(body_markdown(&message, false), "*(no body)*");
    }

    #[test]
    fn quote_depth_counts_various_quote_marker_spacings() {
        assert_eq!(quote_depth("no quote here"), 0);
        assert_eq!(quote_depth(">>> text"), 3);
        assert_eq!(quote_depth("> > > text"), 3);
        assert_eq!(quote_depth(">>  > text"), 3);
        assert_eq!(quote_depth("> text"), 1);
    }

    #[test]
    fn fold_quoted_lines_leaves_unquoted_text_unchanged() {
        let body = "New reply.\n\nSecond paragraph.";
        assert_eq!(fold_quoted_lines(body), body);
    }

    #[test]
    fn fold_quoted_lines_leaves_a_single_quote_level_unchanged() {
        let body = "New reply.\n\n> Quoted line one.\n> Quoted line two.";
        assert_eq!(fold_quoted_lines(body), body);
    }

    #[test]
    fn fold_quoted_lines_folds_a_nested_quote_block_into_a_marker() {
        let body = "New reply.\n\n> Alice wrote:\n>> Original line one.\n>> Original line two.";
        assert_eq!(
            fold_quoted_lines(body),
            "New reply.\n\n> Alice wrote:\n*(2 quoted lines omitted)*"
        );
    }

    #[test]
    fn fold_quoted_lines_folds_both_spaced_and_unspaced_nested_quote_markers() {
        let body = ">> unspaced nested\n> > spaced nested";
        assert_eq!(fold_quoted_lines(body), "*(2 quoted lines omitted)*");
    }

    #[test]
    fn fold_quoted_lines_bridges_an_unprefixed_blank_line_inside_a_nested_quote_run() {
        let body = ">> First nested paragraph.\n\n>> Second nested paragraph.";
        assert_eq!(fold_quoted_lines(body), "*(3 quoted lines omitted)*");
    }

    #[test]
    fn fold_quoted_lines_does_not_bridge_a_blank_line_outside_a_foldable_run() {
        let body = "New reply.\n\n>> Nested quote.";
        assert_eq!(
            fold_quoted_lines(body),
            "New reply.\n\n*(1 quoted line omitted)*"
        );
    }

    #[test]
    fn render_markdown_folds_nested_html_blockquotes_when_fold_quotes_is_true() {
        let raw = b"Subject: Hi\r\nContent-Type: text/html\r\n\r\n\
<p>New reply.</p><blockquote>Alice wrote:<blockquote>Bob's original message.</blockquote></blockquote>";

        let folded = render_markdown(raw, true);
        assert!(folded.contains("New reply."));
        assert!(folded.contains("omitted"));
        assert!(!folded.contains("Bob's original message."));

        let unfolded = render_markdown(raw, false);
        assert!(unfolded.contains("Bob's original message."));
        assert!(!unfolded.contains("omitted"));
    }
}
