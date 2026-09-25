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
//!
//! - **The `Message-ID`.** `mail-builder` always writes one, and with its
//!   `gethostname` feature off its host part is `localhost`. A
//!   [`Composition::message_id_domain`] replaces that with the account's own
//!   domain (#1953).
//!
//! Only `text/plain` bodies are built. `From` is never set: Gmail fills in the
//! authenticated account's address.

use anyhow::{bail, ensure, Result};
use mail_builder::headers::address::Address;
use mail_builder::headers::message_id::MessageId;
use mail_builder::mime::make_boundary;
use mail_builder::MessageBuilder;

use crate::gmail::messages_api::header_value;
use crate::gmail::types::Message;

/// The headers [`ReplyContext::from_message`] reads, for the caller's
/// `messages.get(format=metadata)` request.
pub const REPLY_HEADERS: [&str; 4] = ["Subject", "Message-ID", "References", "In-Reply-To"];

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
        validate_email(email)
            .map_err(|reason| anyhow::anyhow!("invalid address {input:?}: {reason}"))?;
        Ok(Self {
            name,
            email: email.to_string(),
        })
    }

    fn to_address(&self) -> Address<'_> {
        Address::new_address(self.name.as_deref(), self.email.as_str())
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
        Ok(Self {
            thread_id,
            message_id: ids("Message-ID").into_iter().next(),
            references: ids("References"),
            in_reply_to: ids("In-Reply-To"),
            subject: header_value(payload, "Subject").unwrap_or_default(),
        })
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

/// A `text/plain` message to build.
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
    /// The plain-text body.
    pub body: String,
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
/// neither starts nor ends with a hyphen.
fn is_plain_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

impl Composition {
    /// Serialises the message to RFC 5322 bytes with CRLF line endings.
    ///
    /// The `Message-ID` is `<{random}@{message_id_domain}>`. Whether Gmail
    /// keeps it when the draft is sent is unverified (#1953), so it is made
    /// valid either way rather than left as `@localhost`.
    pub fn build(&self) -> Result<Vec<u8>> {
        // A tab is legal (and survives unfolding a long original subject);
        // CR, LF and NUL would break the header.
        ensure!(
            !self.subject.contains(['\r', '\n', '\0']),
            "the subject contains a line break or NUL character"
        );
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders};

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
            }
        );
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
