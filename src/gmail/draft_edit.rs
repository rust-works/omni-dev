//! In-place editing of a stored draft for `gmail draft update` (#1924).
//!
//! `drafts.update` has no partial form: it replaces the whole message. So a
//! field-level edit fetches the draft's RFC 5322 bytes, changes only what was
//! asked for, and uploads the result. Rebuilding the message from parsed
//! fields ([`crate::gmail::compose::Composition`]) would lose everything that
//! builder doesn't model: a draft written in the Gmail UI carries an HTML
//! alternative, headers of its own, and attachments whose exact encoding
//! nothing here needs to touch. [`DraftEdit::apply`] instead edits the bytes
//! **in place**, and everything not named survives byte for byte:
//!
//! - **Header edits** (`To`, `Cc`, `Bcc`, `Subject`) replace just those
//!   header fields, encoded by `mail-builder` exactly as `draft create`
//!   encodes them. The body is not touched.
//! - **Structural edits** (the body, adding or removing attachments) work on
//!   the top-level MIME entity only. For a `multipart/mixed` message the
//!   first non-attachment part is "the body" and every other part is kept
//!   as it is; any other message is itself "the body". New parts are written
//!   by `mail-builder` and the parts are re-joined under a fresh boundary.
//!
//! Every top-level header that isn't a `Content-*` header (`From`, `Date`,
//! `Message-ID`, `In-Reply-To`, `References`, …) is always kept.

use std::borrow::Cow;

use anyhow::{bail, ensure, Result};
use mail_builder::headers::address::Address;
use mail_builder::headers::text::Text;
use mail_builder::headers::Header;
use mail_builder::mime::{BodyPart, MimePart};
use mail_parser::{MessageParser, MimeHeaders};

use crate::gmail::compose::{subject_matches_reply, Attachment, Mailbox, ReplyContext};
use crate::utils::multipart::generate_boundary_absent_from;

/// The changes to make to a draft's message. A `None` or empty field leaves
/// that part of the message exactly as it is.
#[derive(Debug, Clone, Default)]
pub struct DraftEdit {
    /// Replaces the whole `To` list. An empty list removes the header.
    pub to: Option<Vec<Mailbox>>,
    /// Replaces the whole `Cc` list. An empty list removes the header.
    pub cc: Option<Vec<Mailbox>>,
    /// Replaces the whole `Bcc` list. An empty list removes the header.
    pub bcc: Option<Vec<Mailbox>>,
    /// Replaces the `Subject`. An empty subject removes the header.
    pub subject: Option<String>,
    /// Replaces the body with this plain text.
    pub body: Option<String>,
    /// Files to attach after the existing attachments.
    pub attach: Vec<Attachment>,
    /// Filenames of existing attachments to remove.
    pub remove_attachments: Vec<String>,
}

/// The edited message, plus anything the caller should warn about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditedMessage {
    /// The complete new RFC 5322 message.
    pub raw: Vec<u8>,
    /// Human-readable warnings, e.g. that an HTML version was dropped.
    pub warnings: Vec<String>,
}

impl DraftEdit {
    /// Whether the edit changes the MIME structure (the body or the
    /// attachments) rather than only header fields.
    fn is_structural(&self) -> bool {
        self.body.is_some() || !self.attach.is_empty() || !self.remove_attachments.is_empty()
    }

    /// Applies the edit to `original`, a complete RFC 5322 message.
    pub fn apply(&self, original: &[u8]) -> Result<EditedMessage> {
        if let Some(subject) = &self.subject {
            // Same rule as `Composition::build`: a tab is legal, but CR, LF
            // and NUL would break (or inject) a header.
            ensure!(
                !subject.contains(['\r', '\n', '\0']),
                "the subject contains a line break or NUL character"
            );
        }
        for attachment in &self.attach {
            ensure!(
                !attachment.content_type.chars().any(char::is_control),
                "attachment {:?} has an invalid content type",
                attachment.filename
            );
        }

        let split = split_message(original);
        let mut fields = parse_fields(split.headers, split.eol);
        let mut warnings = Vec::new();

        if let Some(subject) = &self.subject {
            if let Some(warning) = reply_subject_warning(original, &fields, subject) {
                warnings.push(warning);
            }
        }
        let eol = split.eol;
        for (name, mailboxes) in [("To", &self.to), ("Cc", &self.cc), ("Bcc", &self.bcc)] {
            if let Some(mailboxes) = mailboxes {
                let field = (!mailboxes.is_empty()).then(|| {
                    let list = Address::new_list(mailboxes.iter().map(to_address).collect());
                    encode_field(name, &list, eol)
                });
                set_field(&mut fields, name, field);
            }
        }
        if let Some(subject) = &self.subject {
            let field =
                (!subject.is_empty()).then(|| encode_field("Subject", &Text::new(subject), eol));
            set_field(&mut fields, "Subject", field);
        }

        let raw = if self.is_structural() {
            self.rebuild_body(original, &split, fields, &mut warnings)?
        } else {
            let mut raw: Vec<u8> = fields
                .iter()
                .flat_map(|f| f.bytes.iter().copied())
                .collect();
            raw.extend_from_slice(split.separator);
            raw.extend_from_slice(split.body);
            raw
        };
        Ok(EditedMessage { raw, warnings })
    }

    /// Rebuilds the top-level entity with the body and attachment edits
    /// applied, keeping every non-`Content-*` header in `fields`.
    fn rebuild_body(
        &self,
        original: &[u8],
        split: &Split<'_>,
        fields: Vec<Field>,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<u8>> {
        let eol = split.eol;
        let (content_fields, mut message_fields): (Vec<Field>, Vec<Field>) =
            fields.into_iter().partition(Field::is_content);

        let TopLevel {
            preamble,
            body,
            others,
        } = top_level_parts(original, split, &content_fields)?;
        let mut others: Vec<(Cow<'_, [u8]>, Option<String>)> = others
            .into_iter()
            .map(|part| {
                let name = part_info(&part).name;
                (part, name)
            })
            .collect();

        let body = match (&self.body, body) {
            (Some(text), old) => {
                if let Some(old) = old {
                    let info = part_info(&old);
                    if info.is_rich_body() {
                        warnings.push(format!(
                            "the draft's {} body is replaced by plain text: its HTML version \
                             (and any inline images) is dropped",
                            info.mime_type
                        ));
                    }
                }
                Some(Cow::Owned(write_part(
                    MimePart::new("text/plain", BodyPart::Text(text.as_str().into())),
                    eol,
                )))
            }
            (None, old) => old,
        };

        for name in &self.remove_attachments {
            let before = others.len();
            others.retain(|(_, part_name)| part_name.as_deref() != Some(name.as_str()));
            if others.len() == before {
                let names: Vec<String> = others
                    .iter()
                    .filter_map(|(_, name)| name.as_ref().map(|n| format!("{n:?}")))
                    .collect();
                if names.is_empty() {
                    bail!("the draft has no attachment named {name:?}: it has no attachments");
                }
                bail!(
                    "the draft has no attachment named {name:?}; its attachments are: {}",
                    names.join(", ")
                );
            }
        }

        let mut parts: Vec<Cow<'_, [u8]>> = body.into_iter().collect();
        parts.extend(others.into_iter().map(|(part, _)| part));
        for attachment in &self.attach {
            let part = MimePart::new(
                attachment.content_type.as_str(),
                BodyPart::Binary(attachment.data.as_slice().into()),
            )
            .attachment(attachment.filename.as_str());
            parts.push(Cow::Owned(write_part(part, eol)));
        }
        if parts.is_empty() {
            // Only an attachment was left, and it was removed.
            parts.push(Cow::Owned(write_part(
                MimePart::new("text/plain", BodyPart::Text("".into())),
                eol,
            )));
        }

        if !message_fields
            .iter()
            .any(|field| field.is_named("MIME-Version"))
        {
            message_fields.push(Field::new("MIME-Version", b"MIME-Version: 1.0", eol));
        }
        let mut raw: Vec<u8> = message_fields
            .iter()
            .flat_map(|f| f.bytes.iter().copied())
            .collect();
        if let [single] = parts.as_slice() {
            // One part is the whole entity: its headers join the message's.
            raw.extend_from_slice(single);
            return Ok(raw);
        }
        let boundary = generate_boundary_absent_from(&parts.concat());
        raw.extend_from_slice(
            format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"").as_bytes(),
        );
        raw.extend_from_slice(eol);
        raw.extend_from_slice(eol);
        raw.extend_from_slice(preamble);
        for part in &parts {
            raw.extend_from_slice(format!("--{boundary}").as_bytes());
            raw.extend_from_slice(eol);
            raw.extend_from_slice(part);
            raw.extend_from_slice(eol);
        }
        raw.extend_from_slice(format!("--{boundary}--").as_bytes());
        raw.extend_from_slice(eol);
        Ok(raw)
    }
}

/// Warns when a subject edit may move a reply draft out of its thread:
/// Gmail files a reply by its subject too, so a draft with `In-Reply-To`
/// whose new subject no longer matches the old one may start a new thread.
fn reply_subject_warning(original: &[u8], fields: &[Field], subject: &str) -> Option<String> {
    if !fields.iter().any(|field| field.is_named("In-Reply-To")) {
        return None;
    }
    let old = MessageParser::default()
        .parse_headers(original)
        .and_then(|message| message.subject().map(str::to_string))
        .unwrap_or_default();
    let reply = ReplyContext {
        subject: old.clone(),
        ..ReplyContext::default()
    };
    (!subject_matches_reply(subject, &reply)).then(|| {
        format!(
            "the new subject differs from the reply's {old:?}; Gmail may move this draft out of \
             its thread"
        )
    })
}

fn to_address(mailbox: &Mailbox) -> Address<'_> {
    Address::new_address(mailbox.name.as_deref(), mailbox.email.as_str())
}

/// `Name: <encoded value>` plus a line ending, in `eol`'s style.
fn encode_field(name: &str, value: &impl Header, eol: &[u8]) -> Field {
    let mut bytes = format!("{name}: ").into_bytes();
    value.write_header(&mut bytes, name.len() + 2);
    // The encoder ends the field (and any fold) with CRLF.
    let bytes = if eol == b"\n" {
        String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .into_bytes()
    } else {
        bytes
    };
    Field {
        name: name.to_string(),
        bytes,
    }
}

/// Replaces the first field called `name` with `field` and drops any other
/// field of that name. With no existing field, `field` is appended. `None`
/// removes the header entirely.
fn set_field(fields: &mut Vec<Field>, name: &str, field: Option<Field>) {
    let mut replacement = field;
    let mut found = false;
    fields.retain_mut(|existing| {
        if !existing.is_named(name) {
            return true;
        }
        found = true;
        match replacement.take() {
            Some(new) => {
                *existing = new;
                true
            }
            None => false,
        }
    });
    if !found {
        fields.extend(replacement);
    }
}

/// A message split at the blank line ending its header section.
struct Split<'a> {
    /// The header section, each field ending in a line ending.
    headers: &'a [u8],
    /// The blank line between headers and body; empty when there is none.
    separator: &'a [u8],
    /// Everything after the separator.
    body: &'a [u8],
    /// The message's line ending: CRLF unless its first line ends in a bare
    /// LF.
    eol: &'static [u8],
}

fn split_message(raw: &[u8]) -> Split<'_> {
    let eol: &'static [u8] = match raw.iter().position(|&b| b == b'\n') {
        Some(nl) if nl == 0 || raw[nl - 1] != b'\r' => b"\n",
        _ => b"\r\n",
    };
    let mut pos = 0;
    while pos < raw.len() {
        let line_end = raw[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(raw.len(), |nl| pos + nl + 1);
        let line = &raw[pos..line_end];
        if line == b"\n" || line == b"\r\n" {
            return Split {
                headers: &raw[..pos],
                separator: line,
                body: &raw[line_end..],
                eol,
            };
        }
        pos = line_end;
    }
    Split {
        headers: raw,
        separator: b"",
        body: b"",
        eol,
    }
}

/// One header field: its name and its complete bytes, folded continuation
/// lines and final line ending included.
#[derive(Debug, Clone)]
struct Field {
    name: String,
    bytes: Vec<u8>,
}

impl Field {
    fn new(name: &str, line: &[u8], eol: &[u8]) -> Self {
        let mut bytes = line.to_vec();
        bytes.extend_from_slice(eol);
        Self {
            name: name.to_string(),
            bytes,
        }
    }

    fn is_named(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    /// Whether this header describes the top-level MIME entity rather than
    /// the message (RFC 2045 §9: every `Content-*` field).
    fn is_content(&self) -> bool {
        self.name
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("content-"))
    }
}

/// Splits a header section into fields. A line starting with a space or tab
/// continues the field before it. A final field without a line ending gets
/// one, so another field can follow it.
fn parse_fields(headers: &[u8], eol: &[u8]) -> Vec<Field> {
    let mut fields: Vec<Field> = Vec::new();
    for line in headers.split_inclusive(|&b| b == b'\n') {
        let continues = matches!(line.first(), Some(b' ' | b'\t'));
        match fields.last_mut() {
            Some(field) if continues => field.bytes.extend_from_slice(line),
            _ => {
                let name = line
                    .iter()
                    .position(|&b| b == b':')
                    .map(|colon| String::from_utf8_lossy(&line[..colon]).trim().to_string())
                    .unwrap_or_default();
                fields.push(Field {
                    name,
                    bytes: line.to_vec(),
                });
            }
        }
    }
    if let Some(last) = fields.last_mut() {
        if !last.bytes.ends_with(b"\n") {
            last.bytes.extend_from_slice(eol);
        }
    }
    fields
}

/// The top-level entity, taken apart for a structural edit.
struct TopLevel<'a> {
    /// A `multipart/mixed` message's preamble, kept as it was.
    preamble: &'a [u8],
    /// "The body": the first non-attachment part, or a non-`mixed` message
    /// whole. Each part is its own headers, a blank line and its body.
    body: Option<Cow<'a, [u8]>>,
    /// Every other part, byte for byte.
    others: Vec<Cow<'a, [u8]>>,
}

fn top_level_parts<'a>(
    original: &'a [u8],
    split: &Split<'a>,
    content_fields: &[Field],
) -> Result<TopLevel<'a>> {
    let root = MessageParser::default().parse_headers(original);
    let boundary = root.as_ref().and_then(|message| {
        let content_type = message.content_type()?;
        let is_mixed = content_type.ctype().eq_ignore_ascii_case("multipart")
            && content_type
                .subtype()
                .is_some_and(|subtype| subtype.eq_ignore_ascii_case("mixed"));
        is_mixed
            .then(|| content_type.attribute("boundary").map(str::to_string))
            .flatten()
    });

    let Some(boundary) = boundary else {
        // Not `multipart/mixed`: the whole entity is the body.
        let mut entity: Vec<u8> = content_fields
            .iter()
            .flat_map(|f| f.bytes.iter().copied())
            .collect();
        entity.extend_from_slice(split.eol);
        entity.extend_from_slice(split.body);
        return Ok(TopLevel {
            preamble: b"",
            body: Some(Cow::Owned(entity)),
            others: Vec::new(),
        });
    };

    let Some((preamble, parts)) = split_multipart(split.body, &boundary) else {
        bail!(
            "the draft is multipart/mixed but its boundary {boundary:?} never occurs in it, so \
             its parts can't be edited; use `--raw` instead"
        );
    };
    let mut parts = parts.into_iter().map(Cow::Borrowed);
    let mut body = None;
    let mut others = Vec::new();
    if let Some(first) = parts.next() {
        if part_info(&first).is_attachment {
            others.push(first);
        } else {
            body = Some(first);
        }
    }
    others.extend(parts);
    Ok(TopLevel {
        preamble,
        body,
        others,
    })
}

/// Splits a multipart body on `--boundary` delimiter lines (RFC 2046
/// §5.1.1), returning the preamble and each part. A part excludes the line
/// ending before the next delimiter, which belongs to the delimiter. A
/// missing close delimiter is tolerated: the last part runs to the end.
/// `None` when the boundary never occurs.
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Option<(&'a [u8], Vec<&'a [u8]>)> {
    let delimiter = format!("--{boundary}");
    let delimiter = delimiter.as_bytes();
    let mut preamble = None;
    let mut parts = Vec::new();
    let mut part_start = 0;
    let mut pos = 0;
    while pos < body.len() {
        let line_end = body[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(body.len(), |nl| pos + nl + 1);
        let line = body[pos..line_end].trim_ascii_end();
        let is_close = line.strip_prefix(delimiter) == Some(b"--");
        if line == delimiter || is_close {
            if preamble.is_none() {
                preamble = Some(&body[..pos]);
            } else {
                let end = if body[..pos].ends_with(b"\r\n") {
                    pos - 2
                } else if body[..pos].ends_with(b"\n") {
                    pos - 1
                } else {
                    pos
                };
                parts.push(&body[part_start..end.max(part_start)]);
            }
            if is_close {
                return Some((preamble.unwrap_or_default(), parts));
            }
            part_start = line_end;
        }
        pos = line_end;
    }
    let preamble = preamble?;
    if part_start < body.len() {
        parts.push(&body[part_start..]);
    }
    Some((preamble, parts))
}

/// What [`top_level_parts`] and the attachment edits need to know about a
/// part.
struct PartInfo {
    /// `type/subtype`, lower-cased; `text/plain` when unstated.
    mime_type: String,
    /// `Content-Disposition: attachment`, or a part that is neither text
    /// nor multipart.
    is_attachment: bool,
    /// The decoded filename (RFC 2231 / RFC 2047), if any.
    name: Option<String>,
}

impl PartInfo {
    /// A body that isn't plain text: HTML, `multipart/alternative`, or
    /// `multipart/related` with inline images.
    fn is_rich_body(&self) -> bool {
        self.mime_type != "text/plain"
    }
}

fn part_info(part: &[u8]) -> PartInfo {
    let parsed = MessageParser::default().parse_headers(part);
    let content_type = parsed.as_ref().and_then(|message| message.content_type());
    let ctype = content_type.map_or_else(|| "text".to_string(), |ct| ct.ctype().to_lowercase());
    let subtype = content_type
        .and_then(|ct| ct.subtype())
        .map_or_else(|| "plain".to_string(), str::to_lowercase);
    let disposition_attachment = parsed
        .as_ref()
        .and_then(|message| message.content_disposition())
        .is_some_and(|disposition| disposition.ctype().eq_ignore_ascii_case("attachment"));
    let is_attachment = disposition_attachment || !matches!(ctype.as_str(), "text" | "multipart");
    PartInfo {
        mime_type: format!("{ctype}/{subtype}"),
        is_attachment,
        name: parsed
            .as_ref()
            .and_then(|message| message.attachment_name())
            .map(str::to_string),
    }
}

/// Serialises a `mail-builder` part, converting its CRLFs to `eol`.
fn write_part(part: MimePart<'_>, eol: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    part.write_part(&mut bytes);
    if eol == b"\n" {
        let mut lf = Vec::with_capacity(bytes.len());
        let mut iter = bytes.iter().peekable();
        while let Some(&b) = iter.next() {
            if b == b'\r' && iter.peek() == Some(&&b'\n') {
                continue;
            }
            lf.push(b);
        }
        lf
    } else {
        bytes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::compose::Composition;
    use mail_parser::Message;

    fn mailbox(input: &str) -> Mailbox {
        Mailbox::parse(input).unwrap()
    }

    fn parse(raw: &[u8]) -> Message<'_> {
        MessageParser::default().parse(raw).unwrap()
    }

    /// The body section: everything after the header section's blank line.
    fn body_of(raw: &[u8]) -> &[u8] {
        split_message(raw).body
    }

    /// A reply draft as the Gmail web UI stores one: `multipart/mixed`
    /// around a `multipart/alternative` body, a PDF and a file with a
    /// non-ASCII (RFC 2231) name, plus headers no builder here models.
    const GMAIL_UI_DRAFT: &str = "MIME-Version: 1.0\r\n\
Date: Mon, 21 Sep 2026 10:00:00 +1000\r\n\
References: <a@example.com> <b@example.com>\r\n\
In-Reply-To: <b@example.com>\r\n\
Message-ID: <draft-1@mail.gmail.com>\r\n\
Subject: Re: Quarterly report\r\n\
From: Me <me@example.com>\r\n\
To: Alice <alice@example.com>\r\n\
Cc: carol@example.com\r\n\
X-Custom-Header: keep me\r\n\
Content-Type: multipart/mixed; boundary=\"000000000000mixed\"\r\n\
\r\n\
--000000000000mixed\r\n\
Content-Type: multipart/alternative; boundary=\"000000000000alt\"\r\n\
\r\n\
--000000000000alt\r\n\
Content-Type: text/plain; charset=\"UTF-8\"\r\n\
\r\n\
Figures attached.\r\n\
\r\n\
--000000000000alt\r\n\
Content-Type: text/html; charset=\"UTF-8\"\r\n\
\r\n\
<div dir=\"ltr\">Figures <b>attached</b>.</div>\r\n\
\r\n\
--000000000000alt--\r\n\
--000000000000mixed\r\n\
Content-Type: application/pdf; name=\"q3.pdf\"\r\n\
Content-Disposition: attachment; filename=\"q3.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
X-Attachment-Id: f_abc\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--000000000000mixed\r\n\
Content-Type: text/plain; charset=\"UTF-8\"; name=\"=?UTF-8?Q?Jahresabschlu=C3=9F=2Etxt?=\"\r\n\
Content-Disposition: attachment; filename*=UTF-8''Jahresabschlu%C3%9F.txt\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
SGVsbG8=\r\n\
--000000000000mixed--\r\n";

    const PDF_PART: &str = "Content-Type: application/pdf; name=\"q3.pdf\"\r\n\
Content-Disposition: attachment; filename=\"q3.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
X-Attachment-Id: f_abc\r\n\
\r\n\
JVBERi0xLjQK";

    const ALT_PART: &str = "Content-Type: multipart/alternative; boundary=\"000000000000alt\"\r\n\
\r\n\
--000000000000alt\r\n\
Content-Type: text/plain; charset=\"UTF-8\"\r\n\
\r\n\
Figures attached.\r\n\
\r\n\
--000000000000alt\r\n\
Content-Type: text/html; charset=\"UTF-8\"\r\n\
\r\n\
<div dir=\"ltr\">Figures <b>attached</b>.</div>\r\n\
\r\n\
--000000000000alt--";

    /// Every header line other than the named ones, in order.
    fn other_fields(raw: &[u8], except: &[&str]) -> Vec<String> {
        let split = split_message(raw);
        parse_fields(split.headers, split.eol)
            .into_iter()
            .filter(|f| !except.iter().any(|name| f.is_named(name)))
            .map(|f| String::from_utf8(f.bytes).unwrap())
            .collect()
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }

    fn attachment(name: &str, content_type: &str, data: &[u8]) -> Attachment {
        Attachment {
            filename: name.to_string(),
            content_type: content_type.to_string(),
            data: data.to_vec(),
        }
    }

    // ── header edits ─────────────────────────────────────────────────

    #[test]
    fn a_subject_edit_changes_only_the_subject_field() {
        let original = GMAIL_UI_DRAFT.as_bytes();
        let edited = DraftEdit {
            subject: Some("RE: quarterly report".to_string()),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();

        assert_eq!(body_of(&edited.raw), body_of(original));
        assert_eq!(
            other_fields(&edited.raw, &["Subject"]),
            other_fields(original, &["Subject"])
        );
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.subject(), Some("RE: quarterly report"));
        // The reply headers survive, and a subject differing only in case
        // still matches the thread's.
        assert_eq!(parsed.in_reply_to().as_text(), Some("b@example.com"));
        assert!(edited.warnings.is_empty(), "{:?}", edited.warnings);
    }

    #[test]
    fn a_recipient_edit_replaces_the_whole_list_in_place() {
        let original = GMAIL_UI_DRAFT.as_bytes();
        let edited = DraftEdit {
            cc: Some(vec![
                mailbox("Zoë <zoe@example.com>"),
                mailbox("dan@example.com"),
            ]),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();

        assert_eq!(body_of(&edited.raw), body_of(original));
        let fields = other_fields(&edited.raw, &[]);
        let original_fields = other_fields(original, &[]);
        // Same position, same neighbours.
        assert_eq!(fields.len(), original_fields.len());
        let cc_index = original_fields
            .iter()
            .position(|f| f.starts_with("Cc:"))
            .unwrap();
        assert!(fields[cc_index].starts_with("Cc: "));
        assert!(fields[cc_index].is_ascii());

        let parsed = parse(&edited.raw);
        let cc: Vec<(Option<&str>, Option<&str>)> = parsed
            .cc()
            .unwrap()
            .iter()
            .map(|a| (a.name(), a.address()))
            .collect();
        assert_eq!(
            cc,
            [
                (Some("Zoë"), Some("zoe@example.com")),
                (None, Some("dan@example.com"))
            ]
        );
    }

    #[test]
    fn a_missing_header_is_added_and_an_empty_list_removes_one() {
        let original = GMAIL_UI_DRAFT.as_bytes();
        let edited = DraftEdit {
            bcc: Some(vec![mailbox("audit@example.com")]),
            cc: Some(Vec::new()),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();
        let parsed = parse(&edited.raw);
        assert_eq!(
            parsed.bcc().unwrap().first().unwrap().address(),
            Some("audit@example.com")
        );
        assert!(parsed.cc().is_none());
        assert_eq!(body_of(&edited.raw), body_of(original));
    }

    #[test]
    fn duplicate_fields_collapse_into_the_replacement() {
        let original = b"To: a@example.com\r\nSubject: One\r\nSubject: Two\r\n\r\nbody\r\n";
        let edited = DraftEdit {
            subject: Some("Three".to_string()),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();
        assert_eq!(
            edited.raw,
            b"To: a@example.com\r\nSubject: Three\r\n\r\nbody\r\n"
        );
    }

    #[test]
    fn folded_fields_are_replaced_whole_and_others_kept_folded() {
        let original = b"To: a@example.com,\r\n b@example.com\r\nX-Long: one\r\n\ttwo\r\n\r\nbody";
        let edited = DraftEdit {
            to: Some(vec![mailbox("c@example.com")]),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();
        assert_eq!(
            edited.raw,
            b"To: <c@example.com>\r\nX-Long: one\r\n\ttwo\r\n\r\nbody"
        );
    }

    #[test]
    fn non_ascii_subjects_are_encoded_words() {
        let edited = DraftEdit {
            subject: Some("Grüße — 你好".to_string()),
            ..DraftEdit::default()
        }
        .apply(b"Subject: x\r\n\r\nbody")
        .unwrap();
        assert!(edited.raw.is_ascii());
        assert_eq!(parse(&edited.raw).subject(), Some("Grüße — 你好"));
    }

    #[test]
    fn a_subject_with_a_line_break_is_rejected() {
        let err = DraftEdit {
            subject: Some("Hi\r\nBcc: eve@example.com".to_string()),
            ..DraftEdit::default()
        }
        .apply(b"Subject: x\r\n\r\nbody")
        .unwrap_err();
        assert!(err.to_string().contains("subject"), "{err}");
    }

    #[test]
    fn changing_a_reply_subject_warns_about_the_thread() {
        let edited = DraftEdit {
            subject: Some("Something else".to_string()),
            ..DraftEdit::default()
        }
        .apply(GMAIL_UI_DRAFT.as_bytes())
        .unwrap();
        assert_eq!(edited.warnings.len(), 1);
        assert!(
            edited.warnings[0].contains("out of its thread"),
            "{:?}",
            edited.warnings
        );

        // A draft that isn't a reply has no thread to fall out of.
        let edited = DraftEdit {
            subject: Some("Something else".to_string()),
            ..DraftEdit::default()
        }
        .apply(b"Subject: x\r\n\r\nbody")
        .unwrap();
        assert!(edited.warnings.is_empty());
    }

    #[test]
    fn lf_only_messages_keep_lf_line_endings() {
        let original = b"To: a@example.com\nSubject: Old\n\nbody\n";
        let edited = DraftEdit {
            subject: Some("New".to_string()),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();
        assert_eq!(edited.raw, b"To: a@example.com\nSubject: New\n\nbody\n");
    }

    #[test]
    fn a_message_without_a_body_gains_the_field() {
        let edited = DraftEdit {
            subject: Some("New".to_string()),
            ..DraftEdit::default()
        }
        .apply(b"To: a@example.com")
        .unwrap();
        assert_eq!(edited.raw, b"To: a@example.com\r\nSubject: New\r\n");
    }

    // ── attachment edits ─────────────────────────────────────────────

    #[test]
    fn removing_an_attachment_keeps_every_other_part_byte_for_byte() {
        let original = GMAIL_UI_DRAFT.as_bytes();
        let edited = DraftEdit {
            remove_attachments: vec!["Jahresabschluß.txt".to_string()],
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();

        assert!(contains(&edited.raw, ALT_PART));
        assert!(contains(&edited.raw, PDF_PART));
        assert!(!contains(&edited.raw, "Jahresabschlu"));
        assert_eq!(
            other_fields(&edited.raw, &["Content-Type"]),
            other_fields(original, &["Content-Type"])
        );
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.attachment_count(), 1);
        assert_eq!(
            parsed.attachment(0).unwrap().attachment_name(),
            Some("q3.pdf")
        );
        assert_eq!(
            parsed.body_text(0).as_deref(),
            Some("Figures attached.\r\n")
        );
        assert!(parsed.body_html(0).unwrap().contains("<b>attached</b>"));
        assert!(edited.warnings.is_empty());
    }

    #[test]
    fn adding_an_attachment_appends_a_part_and_keeps_the_rest() {
        let original = GMAIL_UI_DRAFT.as_bytes();
        let binary: Vec<u8> = (0..=255u8).collect();
        let edited = DraftEdit {
            attach: vec![attachment("data.bin", "application/octet-stream", &binary)],
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();

        assert!(contains(&edited.raw, ALT_PART));
        assert!(contains(&edited.raw, PDF_PART));
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.attachment_count(), 3);
        let added = parsed.attachment(2).unwrap();
        assert_eq!(added.attachment_name(), Some("data.bin"));
        assert_eq!(added.contents(), binary.as_slice());
        assert_eq!(
            parsed.attachment(1).unwrap().attachment_name(),
            Some("Jahresabschluß.txt")
        );
    }

    #[test]
    fn attaching_to_a_single_part_draft_wraps_it_in_multipart_mixed() {
        let original = b"To: a@example.com\r\nSubject: Hi\r\nMIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n\r\nHello.\r\n";
        let edited = DraftEdit {
            attach: vec![attachment("a.pdf", "application/pdf", b"%PDF")],
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();

        // The old entity headers moved into the first part, unchanged.
        assert!(contains(
            &edited.raw,
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n\r\nHello.\r\n"
        ));
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.body_text(0).as_deref(), Some("Hello.\r\n"));
        assert_eq!(parsed.attachment_count(), 1);
        assert_eq!(parsed.attachment(0).unwrap().contents(), b"%PDF");
        assert_eq!(
            other_fields(&edited.raw, &["Content-Type"]),
            [
                "To: a@example.com\r\n",
                "Subject: Hi\r\n",
                "MIME-Version: 1.0\r\n"
            ]
        );
    }

    #[test]
    fn removing_the_last_attachment_unwraps_the_body() {
        let original = Composition {
            to: vec![mailbox("a@example.com")],
            subject: "Hi".to_string(),
            body: "Hello.\n".to_string(),
            attachments: vec![attachment("a.pdf", "application/pdf", b"%PDF")],
            ..Composition::default()
        }
        .build()
        .unwrap();
        let edited = DraftEdit {
            remove_attachments: vec!["a.pdf".to_string()],
            ..DraftEdit::default()
        }
        .apply(&original)
        .unwrap();

        assert!(!contains(&edited.raw, "multipart"));
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.attachment_count(), 0);
        assert_eq!(parsed.body_text(0).as_deref(), Some("Hello.\r\n"));
        assert_eq!(parsed.subject(), Some("Hi"));
    }

    #[test]
    fn removing_an_unknown_attachment_lists_the_real_ones() {
        let err = DraftEdit {
            remove_attachments: vec!["nope.pdf".to_string()],
            ..DraftEdit::default()
        }
        .apply(GMAIL_UI_DRAFT.as_bytes())
        .unwrap_err()
        .to_string();
        assert!(err.contains("\"nope.pdf\""), "{err}");
        assert!(err.contains("\"q3.pdf\", \"Jahresabschluß.txt\""), "{err}");

        let err = DraftEdit {
            remove_attachments: vec!["nope.pdf".to_string()],
            ..DraftEdit::default()
        }
        .apply(b"Subject: x\r\n\r\nbody")
        .unwrap_err()
        .to_string();
        assert!(err.contains("it has no attachments"), "{err}");
    }

    #[test]
    fn an_attachment_content_type_with_a_line_break_is_rejected() {
        let err = DraftEdit {
            attach: vec![attachment("a.txt", "text/plain\r\nX-Evil: 1", b"")],
            ..DraftEdit::default()
        }
        .apply(b"Subject: x\r\n\r\nbody")
        .unwrap_err();
        assert!(err.to_string().contains("content type"), "{err}");
    }

    // ── body edits ───────────────────────────────────────────────────

    #[test]
    fn a_body_edit_keeps_the_attachments_and_warns_that_html_is_dropped() {
        let original = GMAIL_UI_DRAFT.as_bytes();
        let edited = DraftEdit {
            body: Some("Revised figures attached.\n".to_string()),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();

        assert!(contains(&edited.raw, PDF_PART));
        assert!(!contains(&edited.raw, "text/html"));
        let parsed = parse(&edited.raw);
        assert_eq!(
            parsed.body_text(0).as_deref(),
            Some("Revised figures attached.\r\n")
        );
        assert_eq!(parsed.attachment_count(), 2);
        assert_eq!(parsed.in_reply_to().as_text(), Some("b@example.com"));
        assert_eq!(
            other_fields(&edited.raw, &["Content-Type"]),
            other_fields(original, &["Content-Type"])
        );
        assert_eq!(edited.warnings.len(), 1);
        assert!(
            edited.warnings[0].contains("multipart/alternative"),
            "{:?}",
            edited.warnings
        );
    }

    #[test]
    fn a_body_edit_on_a_plain_draft_replaces_it_without_a_warning() {
        let original = Composition {
            to: vec![mailbox("a@example.com")],
            subject: "Hi".to_string(),
            body: "Old.\n".to_string(),
            ..Composition::default()
        }
        .build()
        .unwrap();
        let edited = DraftEdit {
            body: Some("Ünïcödé new body.\n".to_string()),
            ..DraftEdit::default()
        }
        .apply(&original)
        .unwrap();

        assert!(edited.warnings.is_empty(), "{:?}", edited.warnings);
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.body_text(0).as_deref(), Some("Ünïcödé new body.\n"));
        assert_eq!(
            other_fields(&edited.raw, &["Content-Type", "Content-Transfer-Encoding"]),
            other_fields(&original, &["Content-Type", "Content-Transfer-Encoding"])
        );
    }

    #[test]
    fn a_draft_that_is_only_an_attachment_gains_a_body_before_it() {
        let original = b"Subject: x\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\r\n%PDF\r\n--b--\r\n";
        let edited = DraftEdit {
            body: Some("Cover note.".to_string()),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.body_text(0).as_deref(), Some("Cover note."));
        assert_eq!(
            parsed.attachment(0).unwrap().attachment_name(),
            Some("a.pdf")
        );
        assert!(edited.warnings.is_empty());
    }

    #[test]
    fn edits_combine_in_one_pass() {
        let original = GMAIL_UI_DRAFT.as_bytes();
        let edited = DraftEdit {
            to: Some(vec![mailbox("bob@example.com")]),
            body: Some("New.".to_string()),
            attach: vec![attachment("n.txt", "text/plain", b"note")],
            remove_attachments: vec!["q3.pdf".to_string()],
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();
        let parsed = parse(&edited.raw);
        assert_eq!(
            parsed.to().unwrap().first().unwrap().address(),
            Some("bob@example.com")
        );
        assert_eq!(parsed.body_text(0).as_deref(), Some("New."));
        let names: Vec<_> = parsed
            .attachments()
            .map(|a| a.attachment_name().unwrap().to_string())
            .collect();
        assert_eq!(names, ["Jahresabschluß.txt", "n.txt"]);
        assert_eq!(
            parsed.header_raw("X-Custom-Header").map(str::trim),
            Some("keep me")
        );
    }

    #[test]
    fn a_missing_mime_version_is_added_on_a_structural_edit() {
        let edited = DraftEdit {
            attach: vec![attachment("a.pdf", "application/pdf", b"%PDF")],
            ..DraftEdit::default()
        }
        .apply(b"Subject: x\r\n\r\nbody\r\n")
        .unwrap();
        assert_eq!(
            other_fields(&edited.raw, &["Content-Type"]),
            ["Subject: x\r\n", "MIME-Version: 1.0\r\n"]
        );
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.body_text(0).as_deref(), Some("body\r\n"));
        assert_eq!(parsed.attachment_count(), 1);
    }

    #[test]
    fn a_mixed_draft_whose_boundary_is_missing_is_refused() {
        let original =
            b"Content-Type: multipart/mixed; boundary=absent\r\n\r\n--other\r\nx\r\n--other--\r\n";
        let err = DraftEdit {
            body: Some("x".to_string()),
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap_err();
        assert!(err.to_string().contains("--raw"), "{err}");
    }

    #[test]
    fn a_structural_edit_on_an_lf_message_writes_lf_parts() {
        let original = b"Subject: x\nContent-Type: text/plain\n\nbody\n";
        let edited = DraftEdit {
            attach: vec![attachment("a.pdf", "application/pdf", b"%PDF")],
            ..DraftEdit::default()
        }
        .apply(original)
        .unwrap();
        assert!(!edited.raw.contains(&b'\r'));
        let parsed = parse(&edited.raw);
        assert_eq!(parsed.body_text(0).as_deref(), Some("body\n"));
        assert_eq!(parsed.attachment(0).unwrap().contents(), b"%PDF");
    }

    // ── round trips on builder output ────────────────────────────────

    #[test]
    fn round_trip_of_a_composed_multipart_draft_preserves_every_attachment() {
        let binary: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let original = Composition {
            to: vec![mailbox("Zoë Ångström <zoe@example.com>")],
            cc: vec![mailbox("carol@example.com")],
            subject: "Grüße".to_string(),
            body: "Hello.\n".to_string(),
            attachments: vec![
                attachment("data.bin", "application/octet-stream", &binary),
                attachment("Jahresabschluß.pdf", "application/pdf", b"%PDF-1.4"),
            ],
            ..Composition::default()
        }
        .build()
        .unwrap();

        let edited = DraftEdit {
            subject: Some("Grüße (v2)".to_string()),
            body: Some("Hello again.\n".to_string()),
            ..DraftEdit::default()
        }
        .apply(&original)
        .unwrap();

        let before = parse(&original);
        let after = parse(&edited.raw);
        assert_eq!(after.subject(), Some("Grüße (v2)"));
        assert_eq!(after.body_text(0).as_deref(), Some("Hello again.\r\n"));
        assert_eq!(after.attachment_count(), before.attachment_count());
        for (a, b) in after.attachments().zip(before.attachments()) {
            assert_eq!(a.attachment_name(), b.attachment_name());
            assert_eq!(a.contents(), b.contents());
            assert_eq!(
                a.raw_body_offset() - a.raw_header_offset(),
                b.raw_body_offset() - b.raw_header_offset()
            );
        }
        assert_eq!(
            after.to().unwrap().first().unwrap().name(),
            Some("Zoë Ångström")
        );
        assert_eq!(after.date(), before.date());
        assert_eq!(after.message_id(), before.message_id());
    }

    // ── split_multipart ──────────────────────────────────────────────

    #[test]
    fn split_multipart_keeps_the_preamble_and_excludes_delimiter_line_endings() {
        let body = b"preamble\r\n--b\r\nA\r\n--b  \r\n\r\nB\r\n--b--\r\nepilogue";
        let (preamble, parts) = split_multipart(body, "b").unwrap();
        assert_eq!(preamble, b"preamble\r\n");
        assert_eq!(parts, [&b"A"[..], &b"\r\nB"[..]]);
    }

    #[test]
    fn split_multipart_ignores_lines_that_only_start_with_the_delimiter() {
        let body = b"--b\r\n--bx is not a delimiter\r\n--b--";
        let (_, parts) = split_multipart(body, "b").unwrap();
        assert_eq!(parts, [&b"--bx is not a delimiter"[..]]);
    }

    #[test]
    fn split_multipart_tolerates_a_missing_close_delimiter() {
        let (_, parts) = split_multipart(b"--b\nA\n--b\nB\n", "b").unwrap();
        assert_eq!(parts, [&b"A"[..], &b"B\n"[..]]);
        assert!(split_multipart(b"no delimiter", "b").is_none());
    }
}
