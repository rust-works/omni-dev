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

use crate::gmail::attachments::extract_attachments;

/// Renders `raw` as Markdown: a header block (Subject/From/To/Cc/Date/
/// Message-Id/In-Reply-To/References, RFC 2047-decoded courtesy of
/// `mail-parser`), the message body (preferring `text/plain`, falling back
/// to `text/html` converted via `htmd`), and a bullet list of attachment
/// filenames (listed, never embedded — this is a readable rendering, not an
/// export).
///
/// Never fails: an unparseable message degrades to a short placeholder
/// rather than erroring, the same posture
/// [`extract_attachments`] takes so a batch caller like `gmail render`
/// never has one bad file abort the whole run.
pub(crate) fn render_markdown(raw: &[u8]) -> String {
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
    out.push_str(body_markdown(&message).trim_end());
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
fn body_markdown(message: &Message) -> String {
    if message
        .text_part(0)
        .is_some_and(|part| part.is_content_type("text", "plain"))
    {
        if let Some(text) = message.body_text(0) {
            return text.into_owned();
        }
    }
    if let Some(html) = message.body_html(0) {
        return htmd::convert(&html).unwrap_or_else(|_| html.into_owned());
    }
    if let Some(text) = message.body_text(0) {
        return text.into_owned();
    }
    "*(no body)*".to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn render_markdown_renders_plain_text_message() {
        let raw =
            b"Subject: Hello\r\nFrom: Alice <a@example.com>\r\nTo: b@example.com\r\n\r\nHi there.";
        let markdown = render_markdown(raw);
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
        let markdown = render_markdown(raw);
        assert!(markdown.contains("Plain body"));
        assert!(!markdown.contains("HTML body"));
    }

    #[test]
    fn render_markdown_falls_back_to_html_when_no_plain_text_part() {
        let raw = b"Subject: Hi\r\nContent-Type: text/html\r\n\r\n<p>Only <b>HTML</b> here.</p>";
        let markdown = render_markdown(raw);
        assert!(markdown.contains("Only **HTML** here."));
    }

    #[test]
    fn render_markdown_decodes_rfc2047_encoded_subject() {
        let raw = b"Subject: =?utf-8?Q?You=20have=20a=20new=20message?=\r\nFrom: a@example.com\r\n\r\nBody.";
        let markdown = render_markdown(raw);
        assert!(markdown.contains("# You have a new message"));
    }

    #[test]
    fn render_markdown_lists_attachment_filenames_without_embedding_contents() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\n\r\ndata\r\n\
--B--\r\n";
        let markdown = render_markdown(raw);
        assert!(markdown.contains("See attached."));
        assert!(markdown.contains("## Attachments"));
        assert!(markdown.contains("- report.pdf"));
        assert!(!markdown.contains("data\n"));
    }

    #[test]
    fn render_markdown_omits_attachments_section_when_none_present() {
        let raw = b"Subject: Hi\r\nFrom: a@example.com\r\n\r\nJust text.";
        let markdown = render_markdown(raw);
        assert!(!markdown.contains("## Attachments"));
    }

    #[test]
    fn render_markdown_degrades_gracefully_on_unparseable_input() {
        let markdown = render_markdown(b"");
        assert!(!markdown.is_empty());
    }

    #[test]
    fn render_markdown_omits_absent_optional_headers() {
        let raw = b"Subject: Hi\r\nFrom: a@example.com\r\n\r\nBody.";
        let markdown = render_markdown(raw);
        assert!(!markdown.contains("**Cc:**"));
        assert!(!markdown.contains("**In-Reply-To:**"));
        assert!(!markdown.contains("**References:**"));
    }
}
