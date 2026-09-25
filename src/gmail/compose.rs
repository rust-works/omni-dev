//! RFC 5322 message composition for `gmail draft create` (#1923).
//!
//! The encoding itself (RFC 2047 encoded words for non-ASCII subjects and
//! display names, RFC 2231 `filename*=` parameters, `multipart/mixed`
//! assembly and base64 transfer encoding) is `mail-builder`'s. This module
//! owns what that crate leaves to its caller:
//!
//! - **Input validation.** A CR or LF in a subject, display name or address
//!   would let a caller inject a header, so [`Mailbox::parse`] and
//!   [`Composition::build`] reject them outright rather than relying on the
//!   encoder to neutralise them.
//! - **Reply threading.** Gmail only files a draft into an existing thread
//!   when three things agree: the draft's `threadId`, its `In-Reply-To` /
//!   `References` headers, and its `Subject`. [`ReplyContext`] derives the
//!   last two from the original message's headers.
//! - **Reply recipients.** [`ReplyContext::default_recipients`] works out who
//!   a reply goes to when the caller names nobody, as a mail client's Reply
//!   and Reply All do (#1954). The original's address headers are decoded
//!   with `mail-parser`, and every address is re-validated by
//!   [`Mailbox::from_parts`] before it can reach a header.
//!
//! - **The `Message-ID`.** `mail-builder` always writes one, and with its
//!   `gethostname` feature off its host part is `localhost`. A
//!   [`Composition::message_id_domain`] replaces that with the account's own
//!   domain (#1953).
//!
//! - **HTML bodies.** An HTML body always goes out as `multipart/alternative`
//!   beside a plain-text part (#1955). [`plain_text_from_html`] derives that
//!   part when the caller has none, and [`inline_image_warning`] flags HTML
//!   that expects inline images nothing here attaches.
//!
//! `From` is never set: Gmail fills in the authenticated account's address.

use std::collections::HashSet;
use std::fmt;

use anyhow::{bail, ensure, Result};
use mail_builder::headers::address::Address;
use mail_builder::headers::message_id::MessageId;
use mail_builder::mime::make_boundary;
use mail_builder::MessageBuilder;
use mail_parser::MessageParser;

use crate::gmail::messages_api::header_value;
use crate::gmail::types::Message;

/// The headers [`ReplyContext::from_message`] reads, for the caller's
/// `messages.get(format=metadata)` request.
pub const REPLY_HEADERS: [&str; 8] = [
    "Subject",
    "Message-ID",
    "References",
    "In-Reply-To",
    "From",
    "Reply-To",
    "To",
    "Cc",
];

/// Gmail's label on every message the account sent.
const SENT_LABEL: &str = "SENT";

/// One recipient: an address with an optional display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// Display name, if one was given.
    pub name: Option<String>,
    /// The `local@domain` address.
    pub email: String,
}

impl Mailbox {
    /// Parses `addr@example.com` or `Display Name <addr@example.com>`.
    ///
    /// A display name may be double-quoted (`"Doe, Jane" <j@example.com>`);
    /// the quotes are removed and the builder re-quotes or encodes it as
    /// needed. The input is one mailbox, never a comma-separated list, so a
    /// comma inside a name is safe. Rejects control characters (CR/LF would
    /// inject a header), a missing `@`, and whitespace or delimiters inside
    /// the address.
    pub fn parse(input: &str) -> Result<Self> {
        ensure!(
            !input.chars().any(char::is_control),
            "invalid address {input:?}: control characters (such as a line break) are not allowed"
        );
        let trimmed = input.trim();
        let (name, email) = match (trimmed.rfind('<'), trimmed.strip_suffix('>')) {
            (Some(open), Some(without_close)) => {
                let name = unquote(trimmed[..open].trim());
                let email = without_close[open + 1..].trim();
                ((!name.is_empty()).then_some(name), email)
            }
            _ => (None, trimmed),
        };
        Self::checked(name, email)
            .map_err(|reason| anyhow::anyhow!("invalid address {input:?}: {reason}"))
    }

    /// Builds a mailbox from an already-split name and address, such as one
    /// decoded from another message's header.
    ///
    /// Applies the same checks as [`Mailbox::parse`]. The name is checked
    /// *after* decoding, since an RFC 2047 encoded word can carry a CR, LF
    /// or ESC that the raw header never showed. An empty name is `None`.
    pub fn from_parts(name: Option<&str>, email: &str) -> Result<Self> {
        // Trim spaces only, so a stray CR/LF at either end is still seen
        // (and refused) by `checked` rather than quietly trimmed away.
        let name = name
            .map(|name| name.trim_matches(' '))
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        Self::checked(name, email.trim_matches(' '))
            .map_err(|reason| anyhow::anyhow!("invalid address {email:?}: {reason}"))
    }

    /// The checks [`Mailbox::parse`] and [`Mailbox::from_parts`] share: no
    /// control character in the name or address (CR/LF would inject a
    /// header), then [`validate_email`].
    fn checked(name: Option<String>, email: &str) -> std::result::Result<Self, &'static str> {
        if email
            .chars()
            .chain(name.iter().flat_map(|name| name.chars()))
            .any(char::is_control)
        {
            return Err("control characters (such as a line break) are not allowed");
        }
        validate_email(email)?;
        Ok(Self {
            name,
            email: email.to_string(),
        })
    }

    fn to_address(&self) -> Address<'_> {
        Address::new_address(self.name.as_deref(), self.email.as_str())
    }
}

impl fmt::Display for Mailbox {
    /// `Name <addr>`, or the bare address. For messages to a person, not
    /// for a header: the name is not quoted or encoded.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{name} <{}>", self.email),
            None => f.write_str(&self.email),
        }
    }
}

/// Strips one pair of surrounding double quotes and undoes RFC 5322
/// quoted-pairs (`\x` → `x`) in a single left-to-right pass.
fn unquote(name: &str) -> String {
    let Some(inner) = name
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    else {
        return name.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

/// A deliberately loose check: enough to catch typos and anything that would
/// corrupt the header, not a full RFC 5321 validator. Gmail rejects or
/// bounces the rest.
fn validate_email(email: &str) -> std::result::Result<(), &'static str> {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return Err("expected `addr@domain` or `Name <addr@domain>`");
    };
    if local.is_empty() || domain.is_empty() {
        return Err("expected `addr@domain` or `Name <addr@domain>`");
    }
    if email
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"'))
    {
        return Err("the address contains whitespace or a delimiter");
    }
    Ok(())
}

/// A file attached to a composed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The filename recipients see. Non-ASCII is RFC 2231-encoded.
    pub filename: String,
    /// The MIME type, e.g. `application/pdf`.
    pub content_type: String,
    /// The file's bytes, base64-encoded on the wire.
    pub data: Vec<u8>,
}

/// What a reply needs from the message it replies to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplyContext {
    /// Gmail thread id of the original. Goes on the draft's `message`, not
    /// in a header.
    pub thread_id: String,
    /// The original's `Message-ID`, without angle brackets.
    pub message_id: Option<String>,
    /// The original's `References`, without angle brackets, in order.
    pub references: Vec<String>,
    /// The original's `In-Reply-To`, without angle brackets.
    pub in_reply_to: Vec<String>,
    /// The original's `Subject`, or empty.
    pub subject: String,
    /// The original's `From`.
    pub from: Vec<Mailbox>,
    /// The original's `Reply-To`.
    pub reply_to: Vec<Mailbox>,
    /// The original's `To`.
    pub to: Vec<Mailbox>,
    /// The original's `Cc`.
    pub cc: Vec<Mailbox>,
    /// Whether the account sent the original (Gmail's `SENT` label, which
    /// also covers send-as aliases).
    pub sent_by_me: bool,
    /// Addresses in the original's address headers that could not be used,
    /// as `(header, address)`, for the caller to warn about.
    pub skipped: Vec<(&'static str, String)>,
}

impl ReplyContext {
    /// Reads the reply context from a message fetched with
    /// `format=metadata` and [`REPLY_HEADERS`].
    pub fn from_message(message: &Message) -> Result<Self> {
        let Some(thread_id) = message.thread_id.clone().filter(|id| !id.is_empty()) else {
            bail!(
                "message {} has no thread id, so a reply to it can't be filed in its thread",
                message.id
            );
        };
        let payload = message.payload.as_ref();
        let ids = |name| parse_msg_ids(&header_value(payload, name).unwrap_or_default());
        let mut skipped = Vec::new();
        let mut addresses = |name: &'static str| {
            let (mailboxes, bad) = parse_address_header(name, &all_header_values(payload, name));
            skipped.extend(bad.into_iter().map(|bad| (name, bad)));
            mailboxes
        };
        Ok(Self {
            thread_id,
            message_id: ids("Message-ID").into_iter().next(),
            references: ids("References"),
            in_reply_to: ids("In-Reply-To"),
            subject: header_value(payload, "Subject").unwrap_or_default(),
            from: addresses("From"),
            reply_to: addresses("Reply-To"),
            to: addresses("To"),
            cc: addresses("Cc"),
            sent_by_me: message.label_ids.iter().any(|label| label == SENT_LABEL),
            skipped,
        })
    }

    /// The reply's `To` and `Cc` when the caller names nobody (#1954).
    ///
    /// A plain reply goes to the original's `Reply-To`, else its `From`; a
    /// reply to a message the account sent goes to that message's `To`
    /// instead, as in Gmail's UI. `reply_all` adds the original's `To` to the
    /// reply's `To` and its `Cc` to the reply's `Cc`, keeping each recipient
    /// in the header they were in. The addresses in `exclude` (the account's
    /// own, and any the caller has already placed) are left out, as are
    /// repeats, both compared case-insensitively and keeping the first
    /// occurrence (`To` before `Cc`).
    ///
    /// Either list can come back empty, `To` included: deciding what that
    /// means is the caller's job.
    #[must_use]
    pub fn default_recipients(
        &self,
        reply_all: bool,
        exclude: &[String],
    ) -> (Vec<Mailbox>, Vec<Mailbox>) {
        let primary = if self.sent_by_me {
            &self.to
        } else if self.reply_to.is_empty() {
            &self.from
        } else {
            &self.reply_to
        };
        let mut to_candidates: Vec<&Mailbox> = primary.iter().collect();
        let mut cc_candidates: Vec<&Mailbox> = Vec::new();
        if reply_all {
            if !self.sent_by_me {
                to_candidates.extend(&self.to);
            }
            cc_candidates.extend(&self.cc);
        }
        let mut seen: HashSet<String> = exclude.iter().map(|email| email.to_lowercase()).collect();
        let mut keep_new = |candidates: Vec<&Mailbox>| -> Vec<Mailbox> {
            candidates
                .into_iter()
                .filter(|mailbox| seen.insert(mailbox.email.to_lowercase()))
                .cloned()
                .collect()
        };
        let to = keep_new(to_candidates);
        let cc = keep_new(cc_candidates);
        (to, cc)
    }

    /// The reply's `References`: the original's `References` (or, lacking
    /// those, its `In-Reply-To`) followed by the original's `Message-ID`,
    /// per RFC 5322 §3.6.4. Duplicates are dropped, keeping the first.
    #[must_use]
    pub fn reply_references(&self) -> Vec<String> {
        let parents = if self.references.is_empty() {
            &self.in_reply_to
        } else {
            &self.references
        };
        let mut out: Vec<String> = Vec::with_capacity(parents.len() + 1);
        for id in parents.iter().chain(self.message_id.as_ref()) {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    /// `Re: <original subject>`, unless the original already starts with a
    /// `Re:` (any case, `Re :` too), so replies don't stack `Re: Re:`.
    #[must_use]
    pub fn reply_subject(&self) -> String {
        let subject = self.subject.trim();
        if strip_reply_prefixes(subject).len() < subject.len() {
            subject.to_string()
        } else if subject.is_empty() {
            "Re:".to_string()
        } else {
            format!("Re: {subject}")
        }
    }
}

/// `subject` with every leading `Re:` removed (any case, optional
/// whitespace before the colon), then trimmed.
fn strip_reply_prefixes(subject: &str) -> &str {
    let mut rest = subject.trim();
    loop {
        let Some(after_re) = rest
            .get(..2)
            .filter(|prefix| prefix.eq_ignore_ascii_case("re"))
            .map(|_| rest[2..].trim_start())
        else {
            return rest;
        };
        match after_re.strip_prefix(':') {
            Some(after_colon) => rest = after_colon.trim_start(),
            None => return rest,
        }
    }
}

/// Whether `subject` keeps a reply in the original's thread.
///
/// Compares the two subjects without their leading `Re:` prefixes,
/// ignoring case, as Gmail does: both `Re: Report` and `Report` match an
/// original of `Report`.
#[must_use]
pub fn subject_matches_reply(subject: &str, reply: &ReplyContext) -> bool {
    strip_reply_prefixes(subject).eq_ignore_ascii_case(strip_reply_prefixes(&reply.subject))
}

/// Extracts the ids from a `Message-ID` / `References` / `In-Reply-To`
/// value, without their angle brackets. Falls back to whitespace-separated
/// tokens when a malformed value has no brackets at all.
fn parse_msg_ids(value: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        let id = rest[open + 1..open + close].trim();
        if !id.is_empty() {
            ids.push(id.to_string());
        }
        rest = &rest[open + close + 1..];
    }
    if ids.is_empty() && !value.contains('<') {
        ids.extend(value.split_whitespace().map(str::to_string));
    }
    ids
}

/// Every value of the header `name`, joined with `, `. A long recipient
/// list is sometimes split over several `To`/`Cc` headers, and
/// [`header_value`] returns only the first.
fn all_header_values(payload: Option<&serde_json::Value>, name: &str) -> String {
    payload
        .and_then(|payload| payload.get("headers"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|header| {
            header
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .filter_map(|header| header.get("value").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Decodes the address header `name` (one of `From`, `Reply-To`, `To` or
/// `Cc`) with `mail-parser`, which handles RFC 2047 encoded words, quoted
/// names, comments and groups (`Team: a@x, b@y;`, whose members are
/// flattened into the list).
///
/// Returns the usable mailboxes, then the addresses [`Mailbox::from_parts`]
/// refused (as the address, or the name when there is none), for the caller
/// to warn about. An empty group such as `undisclosed-recipients:;` yields
/// neither.
fn parse_address_header(name: &str, value: &str) -> (Vec<Mailbox>, Vec<String>) {
    // Unfold as RFC 5322 does (drop the CRLF, keep the whitespace after it),
    // then turn any stray CR, LF or tab into a space: a folded quoted name
    // would otherwise keep the fold's tab (a control character), and a lone
    // line break must not end the synthetic header early.
    let value: String = value
        .replace("\r\n", "")
        .chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let raw = format!("{name}: {value}\r\n\r\n");
    let Some(parsed) = MessageParser::default().parse_headers(raw.as_bytes()) else {
        return (Vec::new(), Vec::new());
    };
    let address = match name {
        "From" => parsed.from(),
        "Reply-To" => parsed.reply_to(),
        "To" => parsed.to(),
        "Cc" => parsed.cc(),
        _ => None,
    };
    let mut mailboxes = Vec::new();
    let mut skipped = Vec::new();
    for addr in address.into_iter().flat_map(mail_parser::Address::iter) {
        match Mailbox::from_parts(addr.name(), addr.address().unwrap_or_default()) {
            Ok(mailbox) => mailboxes.push(mailbox),
            Err(_) => skipped.push(
                addr.address()
                    .or(addr.name())
                    .unwrap_or_default()
                    .to_string(),
            ),
        }
    }
    (mailboxes, skipped)
}

/// A message to build: `text/plain`, or `multipart/alternative` when it has
/// an HTML body.
#[derive(Debug, Clone, Default)]
pub struct Composition {
    /// `To` recipients.
    pub to: Vec<Mailbox>,
    /// `Cc` recipients.
    pub cc: Vec<Mailbox>,
    /// `Bcc` recipients. Kept as a header: Gmail reads it from the draft
    /// when the draft is sent, and strips it from what recipients receive.
    pub bcc: Vec<Mailbox>,
    /// The `Subject`. May be empty.
    pub subject: String,
    /// The plain-text body, or with `html_body` its plain-text alternative.
    pub body: String,
    /// The HTML body. Sent as `multipart/alternative` after `body`, so
    /// clients that can render HTML show this one.
    pub html_body: Option<String>,
    /// Files attached after the body. Any attachment makes the message
    /// `multipart/mixed`.
    pub attachments: Vec<Attachment>,
    /// When replying: the source of `In-Reply-To` and `References`.
    pub reply: Option<ReplyContext>,
    /// The domain for the generated `Message-ID`, normally the account
    /// address's (see [`message_id_domain`]). `None` leaves the id to
    /// `mail-builder`, which ends it in `@localhost`.
    pub message_id_domain: Option<String>,
}

/// The domain part of `email`, when it can serve as a `Message-ID`'s right
/// half: a dot-separated run of letters, digits and hyphens.
///
/// Anything else (no `@`, an address literal, a stray character) is `None`,
/// so a malformed address can never inject into the header.
#[must_use]
pub fn message_id_domain(email: &str) -> Option<String> {
    let (_, domain) = email.rsplit_once('@')?;
    is_plain_domain(domain).then(|| domain.to_ascii_lowercase())
}

/// Whether `domain` is an RFC 1035 host name: at most 253 characters of
/// dot-separated labels, each 1 to 63 letters, digits and hyphens that
/// neither starts nor ends with a hyphen. The last label may not be all
/// digits (RFC 1123 §2.1), which also rules out a bare IPv4 address.
fn is_plain_domain(domain: &str) -> bool {
    domain.len() <= 253
        && !domain
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()))
        && domain.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Refuses a subject that would break the header.
///
/// [`Composition::build`] runs it too; it is public so a caller can refuse
/// a bad subject before it makes any request.
pub fn check_subject(subject: &str) -> Result<()> {
    // A tab is legal (and survives unfolding a long original subject);
    // CR, LF and NUL would break the header.
    ensure!(
        !subject.contains(['\r', '\n', '\0']),
        "the subject contains a line break or NUL character"
    );
    Ok(())
}

impl Composition {
    /// Serialises the message to RFC 5322 bytes with CRLF line endings.
    ///
    /// The `Message-ID` is `<{random}@{message_id_domain}>`. Whether Gmail
    /// keeps it when the draft is sent is unverified (#1953), so it is made
    /// valid either way rather than left as `@localhost`.
    pub fn build(&self) -> Result<Vec<u8>> {
        check_subject(&self.subject)?;
        for attachment in &self.attachments {
            ensure!(
                !attachment.content_type.chars().any(char::is_control),
                "attachment {:?} has an invalid content type",
                attachment.filename
            );
        }

        let mut builder = MessageBuilder::new();
        if let Some(domain) = &self.message_id_domain {
            ensure!(
                is_plain_domain(domain),
                "invalid Message-ID domain {domain:?}"
            );
            builder = builder.message_id(format!("{}@{domain}", make_boundary(".")));
        }
        for (header, mailboxes) in [("To", &self.to), ("Cc", &self.cc), ("Bcc", &self.bcc)] {
            if mailboxes.is_empty() {
                continue;
            }
            let list = Address::new_list(mailboxes.iter().map(Mailbox::to_address).collect());
            builder = match header {
                "To" => builder.to(list),
                "Cc" => builder.cc(list),
                _ => builder.bcc(list),
            };
        }
        if !self.subject.is_empty() {
            builder = builder.subject(self.subject.as_str());
        }
        if let Some(reply) = &self.reply {
            if let Some(parent) = &reply.message_id {
                builder = builder.in_reply_to(parent.as_str());
            }
            let references = reply.reply_references();
            if !references.is_empty() {
                builder = builder.references(MessageId::new_list(references.into_iter()));
            }
        }
        builder = builder.text_body(self.body.as_str());
        if let Some(html) = &self.html_body {
            builder = builder.html_body(html.as_str());
        }
        for attachment in &self.attachments {
            builder = builder.attachment(
                attachment.content_type.as_str(),
                attachment.filename.as_str(),
                attachment.data.as_slice(),
            );
        }
        builder
            .write_to_vec()
            .map_err(|err| anyhow::anyhow!("Failed to build the message: {err}"))
    }
}

/// The plain-text alternative for an HTML body, as Markdown.
///
/// `htmd` converts it, the converter `gmail render` uses for an HTML-only
/// message. Markdown keeps links as `[text](url)`, which stripping the tags
/// would lose. A full HTML document's `<head>`, `<style>` and `<script>`
/// are left out, so an email template's title and CSS don't open the plain
/// text. A failed conversion is an error, never raw HTML in the plain part.
pub fn plain_text_from_html(html: &str) -> Result<String> {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["head", "title", "style", "script"])
        .build()
        .convert(html)
        .map_err(|err| anyhow::anyhow!("could not derive a plain-text body from the HTML: {err}"))
}

/// A warning when `html` refers to an inline image (`cid:`), which no
/// command here attaches, so the image would show as broken.
///
/// A plain case-insensitive search: it can fire on `cid:` in the text
/// itself, which only costs a spurious warning.
#[must_use]
pub fn inline_image_warning(html: &str) -> Option<String> {
    html.to_ascii_lowercase().contains("cid:").then(|| {
        "the HTML refers to inline images (`cid:`), which aren't attached; they will show as \
         broken images"
            .to_string()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders};

    #[test]
    fn parse_address_header_handles_an_unparsable_header() {
        // An empty header name leaves mail-parser unable to find any header
        // at all, taking the `parse_headers` None branch that a real call
        // site (always a fixed, non-empty header name) never hits.
        assert_eq!(
            parse_address_header("", "alice@example.com"),
            (Vec::new(), Vec::new())
        );
    }

    #[test]
    fn parse_address_header_ignores_an_unrecognized_header_name() {
        // `parse_address_header`'s only call site always passes one of
        // From/Reply-To/To/Cc; a header name outside that set falls through
        // the match's catch-all arm.
        assert_eq!(
            parse_address_header("X-Custom", "alice@example.com"),
            (Vec::new(), Vec::new())
        );
    }

    fn mailbox(input: &str) -> Mailbox {
        Mailbox::parse(input).unwrap()
    }

    fn plain(subject: &str) -> Composition {
        Composition {
            to: vec![mailbox("alice@example.com")],
            subject: subject.to_string(),
            body: "Hello Alice.\n".to_string(),
            ..Composition::default()
        }
    }

    fn built(composition: &Composition) -> String {
        String::from_utf8(composition.build().unwrap()).unwrap()
    }

    /// The raw (still-encoded) value of the first header named `name`,
    /// with folding undone.
    fn raw_header(message: &str, name: &str) -> Option<String> {
        let headers = message.split("\r\n\r\n").next().unwrap();
        let unfolded = headers.replace("\r\n ", " ").replace("\r\n\t", " ");
        unfolded.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    // ── Mailbox::parse ───────────────────────────────────────────────

    #[test]
    fn parse_accepts_a_bare_address() {
        assert_eq!(
            mailbox("  alice@example.com "),
            Mailbox {
                name: None,
                email: "alice@example.com".to_string()
            }
        );
    }

    #[test]
    fn parse_splits_a_display_name_from_the_address() {
        assert_eq!(
            mailbox("Alice Example <alice@example.com>"),
            Mailbox {
                name: Some("Alice Example".to_string()),
                email: "alice@example.com".to_string()
            }
        );
    }

    #[test]
    fn parse_unquotes_a_quoted_name_containing_a_comma() {
        let parsed = mailbox(r#""Doe, Jane \"JD\" \x" <jane@example.com>"#);
        assert_eq!(parsed.name.as_deref(), Some(r#"Doe, Jane "JD" x"#));
        assert_eq!(parsed.email, "jane@example.com");
    }

    #[test]
    fn parse_treats_an_empty_name_as_none() {
        assert_eq!(mailbox("<bob@example.com>").name, None);
    }

    #[test]
    fn parse_rejects_malformed_addresses() {
        for input in [
            "",
            "alice",
            "@example.com",
            "alice@",
            "alice smith@example.com",
            "a@example.com, b@example.com",
            "Alice <alice>",
        ] {
            assert!(Mailbox::parse(input).is_err(), "{input:?} was accepted");
        }
    }

    #[test]
    fn parse_rejects_line_breaks_that_would_inject_a_header() {
        for input in [
            "alice@example.com\r\nBcc: eve@example.com",
            "Alice\n<alice@example.com>",
        ] {
            let err = Mailbox::parse(input).unwrap_err().to_string();
            assert!(err.contains("control characters"), "{err}");
        }
    }

    // ── Composition::build ───────────────────────────────────────────

    #[test]
    fn build_plain_message_is_a_single_text_part_without_from() {
        let message = built(&plain("Hello"));
        assert_eq!(
            raw_header(&message, "To").as_deref(),
            Some("<alice@example.com>")
        );
        assert_eq!(raw_header(&message, "Subject").as_deref(), Some("Hello"));
        assert!(raw_header(&message, "From").is_none());
        assert!(raw_header(&message, "Date").is_some());
        assert!(raw_header(&message, "MIME-Version").is_some());
        // With no domain, the id is the builder's own placeholder (#1953).
        assert!(raw_header(&message, "Message-ID")
            .unwrap()
            .ends_with("@localhost>"));
        assert!(raw_header(&message, "Content-Type")
            .unwrap()
            .starts_with("text/plain"));
        assert!(!message.contains("multipart"));

        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        // A 7-bit body goes out with RFC 5322's CRLF line endings.
        assert_eq!(parsed.body_text(0).as_deref(), Some("Hello Alice.\r\n"));
    }

    fn with_domain(domain: &str) -> Composition {
        Composition {
            message_id_domain: Some(domain.to_string()),
            ..plain("Hello")
        }
    }

    #[test]
    fn build_writes_a_message_id_in_the_given_domain() {
        let message = built(&with_domain("example.org"));
        assert_eq!(message.matches("Message-ID:").count(), 1, "{message}");
        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        let id = parsed.message_id().unwrap();
        let (left, right) = id.split_once('@').unwrap();
        assert_eq!(right, "example.org");
        // RFC 5322 `id-left` is a `dot-atom-text`.
        assert!(!left.is_empty() && !left.starts_with('.') && !left.ends_with('.'));
        assert!(left.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'));
    }

    #[test]
    fn build_writes_a_fresh_message_id_each_time() {
        let composition = with_domain("example.org");
        assert_ne!(
            raw_header(&built(&composition), "Message-ID"),
            raw_header(&built(&composition), "Message-ID")
        );
    }

    #[test]
    fn build_rejects_a_message_id_domain_that_could_inject() {
        for domain in ["", "example.org>\r\nBcc: eve@example.com", "a..b", "a b"] {
            let err = with_domain(domain).build().unwrap_err().to_string();
            assert!(err.contains("invalid Message-ID domain"), "{err}");
        }
    }

    #[test]
    fn message_id_domain_takes_the_part_after_the_last_at() {
        assert_eq!(
            message_id_domain("Me@Example.ORG").as_deref(),
            Some("example.org")
        );
        assert_eq!(
            message_id_domain("\"a@b\"@mail.example.com").as_deref(),
            Some("mail.example.com")
        );
        let longest = format!("me@{}.org", "a".repeat(63));
        assert!(message_id_domain(&longest).is_some());
        let long_label = format!("me@{}.org", "a".repeat(64));
        let long_domain = format!("me@{}org", "a.".repeat(126));
        for email in [
            "no-at-sign",
            "me@",
            "me@[127.0.0.1]",
            "me@127.0.0.1",
            "me@a..b",
            "me@exa mple.org",
            "me@-example.org",
            "me@example-.org",
            &long_label,
            &long_domain,
        ] {
            assert_eq!(message_id_domain(email), None, "{email}");
        }
    }

    #[test]
    fn build_writes_cc_and_bcc_headers() {
        let composition = Composition {
            cc: vec![mailbox("Carol <carol@example.com>")],
            bcc: vec![mailbox("dave@example.com"), mailbox("erin@example.com")],
            ..plain("Hi")
        };
        let parsed_bytes = composition.build().unwrap();
        let parsed = MessageParser::default().parse(&parsed_bytes).unwrap();
        let emails = |list: Option<&mail_parser::Address<'_>>| -> Vec<String> {
            list.unwrap()
                .iter()
                .map(|a| a.address().unwrap().to_string())
                .collect()
        };
        assert_eq!(emails(parsed.cc()), ["carol@example.com"]);
        assert_eq!(
            emails(parsed.bcc()),
            ["dave@example.com", "erin@example.com"]
        );
        assert_eq!(parsed.cc().unwrap().first().unwrap().name(), Some("Carol"));
    }

    #[test]
    fn build_omits_empty_recipient_headers_and_subject() {
        let composition = Composition {
            subject: String::new(),
            ..plain("")
        };
        let message = built(&composition);
        assert!(raw_header(&message, "Cc").is_none());
        assert!(raw_header(&message, "Bcc").is_none());
        assert!(raw_header(&message, "Subject").is_none());
    }

    #[test]
    fn build_encodes_non_ascii_subject_and_name_as_rfc2047_encoded_words() {
        let composition = Composition {
            to: vec![mailbox("Zoë Ångström <zoe@example.com>")],
            ..plain("Grüße — 你好")
        };
        let message = built(&composition);
        // The wire form is pure ASCII encoded words…
        assert!(message.is_ascii());
        assert!(raw_header(&message, "Subject")
            .unwrap()
            .starts_with("=?utf-8?"));
        assert!(raw_header(&message, "To").unwrap().contains("=?utf-8?"));
        // …which decode back to the originals.
        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(parsed.subject(), Some("Grüße — 你好"));
        let to = parsed.to().unwrap().first().unwrap();
        assert_eq!(to.name(), Some("Zoë Ångström"));
        assert_eq!(to.address(), Some("zoe@example.com"));
    }

    #[test]
    fn build_encodes_a_non_ascii_body_losslessly() {
        let composition = Composition {
            body: "Ça va? Ünïcödé body.\n".to_string(),
            ..plain("Hi")
        };
        let message = built(&composition);
        assert!(message.is_ascii());
        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(
            parsed.body_text(0).as_deref(),
            Some("Ça va? Ünïcödé body.\n")
        );
    }

    #[test]
    fn build_with_attachments_is_multipart_mixed_with_base64_parts() {
        let binary: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let composition = Composition {
            attachments: vec![
                Attachment {
                    filename: "data.bin".to_string(),
                    content_type: "application/octet-stream".to_string(),
                    data: binary.clone(),
                },
                Attachment {
                    filename: "Jahresabschluß.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    data: b"%PDF-1.4".to_vec(),
                },
            ],
            ..plain("With files")
        };
        let message = built(&composition);
        assert!(raw_header(&message, "Content-Type")
            .unwrap()
            .starts_with("multipart/mixed"));
        assert!(message.contains("Content-Transfer-Encoding: base64"));

        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(parsed.body_text(0).as_deref(), Some("Hello Alice.\r\n"));
        assert_eq!(parsed.attachment_count(), 2);
        let first = parsed.attachment(0).unwrap();
        assert_eq!(first.attachment_name(), Some("data.bin"));
        assert_eq!(first.contents(), binary.as_slice());
        let second = parsed.attachment(1).unwrap();
        assert_eq!(second.attachment_name(), Some("Jahresabschluß.pdf"));
        assert_eq!(second.content_type().unwrap().ctype(), "application");
        assert_eq!(second.content_type().unwrap().subtype(), Some("pdf"));
        assert_eq!(second.contents(), b"%PDF-1.4");
    }

    fn with_html(html: &str) -> Composition {
        Composition {
            html_body: Some(html.to_string()),
            ..plain("Styled")
        }
    }

    /// Byte offset of the first `Content-Type: {mime_type}` line.
    fn content_type_at(message: &str, mime_type: &str) -> usize {
        message
            .find(&format!("Content-Type: {mime_type}"))
            .unwrap_or_else(|| panic!("no {mime_type} part in {message}"))
    }

    #[test]
    fn build_with_html_is_multipart_alternative_plain_text_first() {
        let message = built(&with_html("<p>Hello <b>Alice</b>.</p>"));
        assert!(raw_header(&message, "Content-Type")
            .unwrap()
            .starts_with("multipart/alternative"));
        // Clients show the last alternative they can render, so HTML goes last.
        assert!(content_type_at(&message, "text/plain") < content_type_at(&message, "text/html"));
        assert!(!message.contains("multipart/mixed"));

        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(parsed.body_text(0).as_deref(), Some("Hello Alice.\r\n"));
        assert_eq!(
            parsed.body_html(0).as_deref(),
            Some("<p>Hello <b>Alice</b>.</p>")
        );
    }

    #[test]
    fn build_with_html_and_an_attachment_nests_the_alternative_in_mixed() {
        let composition = Composition {
            attachments: vec![Attachment {
                filename: "q3.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                data: b"%PDF-1.4".to_vec(),
            }],
            ..with_html("<p>Figures attached.</p>")
        };
        let message = built(&composition);
        assert!(raw_header(&message, "Content-Type")
            .unwrap()
            .starts_with("multipart/mixed"));
        let alternative = content_type_at(&message, "multipart/alternative");
        assert!(alternative < content_type_at(&message, "text/plain"));
        assert!(
            content_type_at(&message, "text/html") < content_type_at(&message, "application/pdf")
        );

        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(parsed.body_text(0).as_deref(), Some("Hello Alice.\r\n"));
        assert_eq!(
            parsed.body_html(0).as_deref(),
            Some("<p>Figures attached.</p>")
        );
        assert_eq!(parsed.attachment_count(), 1);
        assert_eq!(parsed.attachment(0).unwrap().contents(), b"%PDF-1.4");
    }

    #[test]
    fn build_encodes_a_non_ascii_html_body_losslessly() {
        let message = built(&with_html("<p>Grüße, “Ünïcödé” — ✓</p>"));
        assert!(message.is_ascii());
        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(
            parsed.body_html(0).as_deref(),
            Some("<p>Grüße, “Ünïcödé” — ✓</p>")
        );
    }

    #[test]
    fn build_reply_with_html_keeps_the_threading_headers() {
        let composition = Composition {
            reply: Some(ReplyContext {
                thread_id: "t".to_string(),
                message_id: Some("b@example.com".to_string()),
                ..ReplyContext::default()
            }),
            ..with_html("<p>Thanks!</p>")
        };
        let message = built(&composition);
        assert_eq!(
            raw_header(&message, "In-Reply-To").as_deref(),
            Some("<b@example.com>")
        );
        assert_eq!(
            raw_header(&message, "References").as_deref(),
            Some("<b@example.com>")
        );
    }

    #[test]
    fn plain_text_from_html_keeps_links_and_emphasis_as_markdown() {
        let text = plain_text_from_html(
            "<p>See <a href=\"https://example.com/q3\">the report</a>, <b>today</b>.</p>",
        )
        .unwrap();
        assert_eq!(text, "See [the report](https://example.com/q3), **today**.");
    }

    #[test]
    fn plain_text_from_html_leaves_out_a_documents_head_styles_and_scripts() {
        let text = plain_text_from_html(
            "<!DOCTYPE html><html><head><title>Q3 Note</title>\
             <style>p { color: red }</style></head>\
             <body><style>.x { y: z }</style><p>Hello <b>Alice</b>.</p>\
             <script>alert(1)</script></body></html>",
        )
        .unwrap();
        assert_eq!(text, "Hello **Alice**.");
    }

    #[test]
    fn inline_image_warning_fires_on_cid_references_in_any_case() {
        assert!(inline_image_warning("<img src=\"CID:logo@x\">")
            .unwrap()
            .contains("inline images"));
        assert!(inline_image_warning("<div style=\"background: url(cid:bg)\">").is_some());
        assert_eq!(
            inline_image_warning("<img src=\"https://example.com/a.png\">"),
            None
        );
    }

    #[test]
    fn build_rejects_a_subject_with_a_line_break() {
        let err = plain("Hi\r\nBcc: eve@example.com")
            .build()
            .unwrap_err()
            .to_string();
        assert!(err.contains("subject"), "{err}");
    }

    #[test]
    fn build_rejects_an_attachment_content_type_with_a_line_break() {
        let composition = Composition {
            attachments: vec![Attachment {
                filename: "a.txt".to_string(),
                content_type: "text/plain\r\nX-Evil: 1".to_string(),
                data: Vec::new(),
            }],
            ..plain("Hi")
        };
        assert!(composition.build().is_err());
    }

    // ── Reply threading ──────────────────────────────────────────────

    fn original(headers: &[(&str, &str)]) -> Message {
        let headers: Vec<serde_json::Value> = headers
            .iter()
            .map(|(name, value)| serde_json::json!({"name": name, "value": value}))
            .collect();
        Message {
            id: "m-orig".to_string(),
            thread_id: Some("t-orig".to_string()),
            payload: Some(serde_json::json!({ "headers": headers })),
            ..Message::default()
        }
    }

    #[test]
    fn reply_context_reads_ids_without_angle_brackets() {
        let reply = ReplyContext::from_message(&original(&[
            ("Subject", "Quarterly report"),
            ("Message-Id", "<c@example.com>"),
            ("References", "<a@example.com>\r\n <b@example.com>"),
            ("In-Reply-To", "<b@example.com>"),
        ]))
        .unwrap();
        assert_eq!(
            reply,
            ReplyContext {
                thread_id: "t-orig".to_string(),
                message_id: Some("c@example.com".to_string()),
                references: vec!["a@example.com".to_string(), "b@example.com".to_string()],
                in_reply_to: vec!["b@example.com".to_string()],
                subject: "Quarterly report".to_string(),
                ..ReplyContext::default()
            }
        );
    }

    // ── Mailbox::from_parts ──────────────────────────────────────────

    #[test]
    fn from_parts_trims_and_drops_an_empty_name() {
        assert_eq!(
            Mailbox::from_parts(Some("  "), " bob@example.com ").unwrap(),
            Mailbox {
                name: None,
                email: "bob@example.com".to_string()
            }
        );
        assert_eq!(
            Mailbox::from_parts(Some(" Bob "), "bob@example.com")
                .unwrap()
                .name
                .as_deref(),
            Some("Bob")
        );
    }

    #[test]
    fn from_parts_rejects_control_characters_and_bad_addresses() {
        for (name, email) in [
            (Some("Eve\r\nBcc: x@example.com"), "eve@example.com"),
            (Some("Eve\u{1b}[31m"), "eve@example.com"),
            (None, "eve@example.com\n"),
            (None, ""),
            (None, "no-at-sign"),
        ] {
            assert!(
                Mailbox::from_parts(name, email).is_err(),
                "{name:?} {email:?} was accepted"
            );
        }
    }

    #[test]
    fn mailbox_displays_name_and_address() {
        assert_eq!(mailbox("Alice <a@x.org>").to_string(), "Alice <a@x.org>");
        assert_eq!(mailbox("a@x.org").to_string(), "a@x.org");
    }

    // ── Reply recipients ─────────────────────────────────────────────

    fn emails(mailboxes: &[Mailbox]) -> Vec<&str> {
        mailboxes.iter().map(|m| m.email.as_str()).collect()
    }

    fn recipients_of(headers: &[(&str, &str)]) -> ReplyContext {
        ReplyContext::from_message(&original(headers)).unwrap()
    }

    #[test]
    fn reply_context_reads_the_address_headers_and_sent_label() {
        let mut message = original(&[
            ("From", "Alice <alice@example.com>"),
            ("Reply-To", "list@example.com"),
            ("To", "me@example.org, Bob <bob@example.com>"),
            ("Cc", "carol@example.com"),
        ]);
        message.label_ids = vec!["INBOX".to_string(), "SENT".to_string()];
        let reply = ReplyContext::from_message(&message).unwrap();
        assert_eq!(reply.from, [mailbox("Alice <alice@example.com>")]);
        assert_eq!(emails(&reply.reply_to), ["list@example.com"]);
        assert_eq!(emails(&reply.to), ["me@example.org", "bob@example.com"]);
        assert_eq!(emails(&reply.cc), ["carol@example.com"]);
        assert!(reply.sent_by_me);
        assert!(reply.skipped.is_empty());
        assert!(!recipients_of(&[]).sent_by_me);
    }

    #[test]
    fn reply_context_decodes_encoded_words_and_quoted_names() {
        let reply = recipients_of(&[
            (
                "From",
                "=?UTF-8?Q?Zo=C3=AB_=C3=85ngstr=C3=B6m?= <zoe@example.com>",
            ),
            (
                "To",
                r#""Doe, Jane" <jane@example.com>, bob@example.com (Bob)"#,
            ),
        ]);
        assert_eq!(reply.from[0].name.as_deref(), Some("Zoë Ångström"));
        assert_eq!(reply.from[0].email, "zoe@example.com");
        assert_eq!(reply.to[0].name.as_deref(), Some("Doe, Jane"));
        assert_eq!(emails(&reply.to), ["jane@example.com", "bob@example.com"]);
    }

    #[test]
    fn reply_context_flattens_groups_and_unfolds() {
        let reply = recipients_of(&[
            (
                "To",
                "Team: a@example.com,\r\n b@example.com;, c@example.com",
            ),
            ("Cc", "undisclosed-recipients:;"),
        ]);
        assert_eq!(
            emails(&reply.to),
            ["a@example.com", "b@example.com", "c@example.com"]
        );
        assert!(reply.cc.is_empty());
        assert!(reply.skipped.is_empty(), "{:?}", reply.skipped);
    }

    #[test]
    fn reply_context_skips_an_encoded_line_break_in_a_name() {
        // `=0D=0A` decodes to CR LF: kept, it would inject a header.
        let reply = recipients_of(&[(
            "Cc",
            "=?UTF-8?Q?Eve=0D=0ABcc:_x@example.com?= <eve@example.com>, ok@example.com",
        )]);
        assert_eq!(emails(&reply.cc), ["ok@example.com"]);
        assert_eq!(reply.skipped, [("Cc", "eve@example.com".to_string())]);
    }

    #[test]
    fn reply_context_reads_every_repeated_address_header() {
        let reply = recipients_of(&[
            ("Cc", "a@example.com"),
            ("To", "t@example.com"),
            ("cc", "b@example.com, c@example.com"),
        ]);
        assert_eq!(
            emails(&reply.cc),
            ["a@example.com", "b@example.com", "c@example.com"]
        );
    }

    #[test]
    fn reply_context_unfolds_a_tab_in_a_quoted_name() {
        let reply = recipients_of(&[("To", "\"Doe,\r\n\tJane\" <jane@example.com>")]);
        assert_eq!(reply.to.len(), 1, "{:?}", reply.skipped);
        assert_eq!(reply.to[0].name.as_deref(), Some("Doe, Jane"));
    }

    #[test]
    fn reply_context_keeps_a_raw_line_break_inside_its_header() {
        // However `mail-parser` reads the garbage, it stays a `From` value:
        // it can't start a `Cc` of its own.
        let reply = recipients_of(&[("From", "a@example.com\r\nCc: eve@example.com")]);
        assert!(reply.cc.is_empty());
        assert!(reply.from.len() <= 1, "{:?}", reply.from);
    }

    fn context(from: &[&str], reply_to: &[&str], to: &[&str], cc: &[&str]) -> ReplyContext {
        let list = |values: &[&str]| values.iter().map(|v| mailbox(v)).collect();
        ReplyContext {
            from: list(from),
            reply_to: list(reply_to),
            to: list(to),
            cc: list(cc),
            ..ReplyContext::default()
        }
    }

    #[test]
    fn default_recipients_prefers_reply_to_over_from() {
        let reply = context(&["a@x.org"], &["list@x.org", "b@x.org"], &["me@x.org"], &[]);
        let (to, cc) = reply.default_recipients(false, &[]);
        assert_eq!(emails(&to), ["list@x.org", "b@x.org"]);
        assert!(cc.is_empty());
    }

    #[test]
    fn default_recipients_falls_back_to_from() {
        let reply = context(&["Alice <a@x.org>"], &[], &["me@x.org"], &["c@x.org"]);
        let (to, cc) = reply.default_recipients(false, &[]);
        assert_eq!(to, [mailbox("Alice <a@x.org>")]);
        assert!(cc.is_empty());
    }

    #[test]
    fn default_recipients_of_a_sent_message_go_to_its_to() {
        let reply = ReplyContext {
            sent_by_me: true,
            ..context(&["me@x.org"], &[], &["b@x.org", "c@x.org"], &["d@x.org"])
        };
        let (to, cc) = reply.default_recipients(false, &[]);
        assert_eq!(emails(&to), ["b@x.org", "c@x.org"]);
        assert!(cc.is_empty());
        let (to, cc) = reply.default_recipients(true, &["me@x.org".to_string()]);
        assert_eq!(emails(&to), ["b@x.org", "c@x.org"]);
        assert_eq!(emails(&cc), ["d@x.org"]);
    }

    #[test]
    fn default_recipients_reply_all_keeps_each_recipient_in_its_header() {
        let reply = context(&["a@x.org"], &[], &["me@x.org", "b@x.org"], &["c@x.org"]);
        let (to, cc) = reply.default_recipients(true, &["me@x.org".to_string()]);
        assert_eq!(emails(&to), ["a@x.org", "b@x.org"]);
        assert_eq!(emails(&cc), ["c@x.org"]);
    }

    #[test]
    fn default_recipients_excludes_addresses_case_insensitively() {
        let reply = context(
            &["a@x.org"],
            &[],
            &["Me@X.org", "b@x.org"],
            &["ALIAS@y.org", "c@x.org"],
        );
        let own = ["me@x.org".to_string(), "alias@Y.org".to_string()];
        let (to, cc) = reply.default_recipients(true, &own);
        assert_eq!(emails(&to), ["a@x.org", "b@x.org"]);
        assert_eq!(emails(&cc), ["c@x.org"]);
    }

    #[test]
    fn default_recipients_dedupes_across_to_and_cc() {
        let reply = context(
            &["a@x.org"],
            &[],
            &["A@x.org", "b@x.org", "b@x.org"],
            &["B@X.ORG", "c@x.org", "a@x.org"],
        );
        let (to, cc) = reply.default_recipients(true, &[]);
        assert_eq!(emails(&to), ["a@x.org", "b@x.org"]);
        assert_eq!(emails(&cc), ["c@x.org"]);
    }

    #[test]
    fn default_recipients_can_be_empty() {
        let (to, cc) = ReplyContext::default().default_recipients(true, &[]);
        assert!(to.is_empty() && cc.is_empty());
        // Replying to your own message sent only to yourself.
        let reply = ReplyContext {
            sent_by_me: true,
            ..context(&["me@x.org"], &[], &["me@x.org"], &[])
        };
        assert!(reply
            .default_recipients(true, &["me@x.org".to_string()])
            .0
            .is_empty());
    }

    #[test]
    fn reply_context_requires_a_thread_id() {
        let message = Message {
            thread_id: None,
            ..original(&[])
        };
        assert!(ReplyContext::from_message(&message).is_err());
    }

    #[test]
    fn reply_references_appends_the_parent_to_its_references() {
        let reply = ReplyContext {
            message_id: Some("c@x".to_string()),
            references: vec!["a@x".to_string(), "b@x".to_string()],
            in_reply_to: vec!["b@x".to_string()],
            ..ReplyContext::default()
        };
        assert_eq!(reply.reply_references(), ["a@x", "b@x", "c@x"]);
    }

    #[test]
    fn reply_references_falls_back_to_in_reply_to_and_dedupes() {
        let reply = ReplyContext {
            message_id: Some("c@x".to_string()),
            in_reply_to: vec!["b@x".to_string(), "c@x".to_string()],
            ..ReplyContext::default()
        };
        assert_eq!(reply.reply_references(), ["b@x", "c@x"]);
    }

    #[test]
    fn reply_references_of_a_first_message_is_just_its_id() {
        let reply = ReplyContext {
            message_id: Some("c@x".to_string()),
            ..ReplyContext::default()
        };
        assert_eq!(reply.reply_references(), ["c@x"]);
    }

    #[test]
    fn reply_subject_prefixes_re_once() {
        let subject = |s: &str| {
            ReplyContext {
                subject: s.to_string(),
                ..ReplyContext::default()
            }
            .reply_subject()
        };
        assert_eq!(subject("Report"), "Re: Report");
        assert_eq!(subject("  Report  "), "Re: Report");
        assert_eq!(subject("Re: Report"), "Re: Report");
        assert_eq!(subject("RE: Report"), "RE: Report");
        assert_eq!(subject("re:Report"), "re:Report");
        assert_eq!(subject("RE : Report"), "RE : Report");
        assert_eq!(subject("Regarding"), "Re: Regarding");
        assert_eq!(subject(""), "Re:");
        assert_eq!(subject("Ré"), "Re: Ré");
    }

    #[test]
    fn subject_matches_reply_ignores_case_and_whitespace() {
        let reply = ReplyContext {
            subject: "Report".to_string(),
            ..ReplyContext::default()
        };
        assert!(subject_matches_reply(" re: report ", &reply));
        assert!(subject_matches_reply("Report", &reply));
        assert!(subject_matches_reply("Re: Re: REPORT", &reply));
        assert!(!subject_matches_reply("Re: Other", &reply));
        assert!(!subject_matches_reply("Reports", &reply));
    }

    #[test]
    fn strip_reply_prefixes_removes_only_leading_re_colons() {
        assert_eq!(strip_reply_prefixes("Re: RE :re:x"), "x");
        assert_eq!(strip_reply_prefixes("Regarding"), "Regarding");
        assert_eq!(strip_reply_prefixes("Re"), "Re");
        assert_eq!(strip_reply_prefixes("Ré: x"), "Ré: x");
        assert_eq!(strip_reply_prefixes("  "), "");
    }

    #[test]
    fn build_accepts_a_tab_in_the_subject() {
        let message = built(&plain("Re: Quarterly\treport"));
        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert!(parsed.subject().unwrap().starts_with("Re: Quarterly"));
    }

    #[test]
    fn parse_msg_ids_handles_folding_and_missing_brackets() {
        assert_eq!(
            parse_msg_ids("<a@x> <b@x>\r\n\t<c@x>"),
            ["a@x", "b@x", "c@x"]
        );
        assert_eq!(parse_msg_ids("a@x b@x"), ["a@x", "b@x"]);
        assert!(parse_msg_ids("").is_empty());
        assert!(parse_msg_ids("<>").is_empty());
        // An unclosed `<` stops the scan without panicking or looping;
        // ids found before it are kept.
        assert!(parse_msg_ids("<unclosed").is_empty());
        assert_eq!(parse_msg_ids("<a@x> <unclosed"), ["a@x"]);
    }

    #[test]
    fn build_reply_writes_in_reply_to_and_references() {
        let composition = Composition {
            reply: Some(ReplyContext {
                thread_id: "t-orig".to_string(),
                message_id: Some("c@example.com".to_string()),
                references: vec!["a@example.com".to_string()],
                in_reply_to: Vec::new(),
                subject: "Report".to_string(),
                ..ReplyContext::default()
            }),
            ..plain("Re: Report")
        };
        let message = built(&composition);
        assert_eq!(
            raw_header(&message, "In-Reply-To").as_deref(),
            Some("<c@example.com>")
        );
        assert_eq!(
            raw_header(&message, "References").as_deref(),
            Some("<a@example.com> <c@example.com>")
        );
    }

    #[test]
    fn build_reply_to_a_message_without_an_id_writes_no_threading_headers() {
        let composition = Composition {
            reply: Some(ReplyContext {
                thread_id: "t".to_string(),
                ..ReplyContext::default()
            }),
            ..plain("Re: x")
        };
        let message = built(&composition);
        assert!(raw_header(&message, "In-Reply-To").is_none());
        assert!(raw_header(&message, "References").is_none());
    }
}
