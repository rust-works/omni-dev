//! CLI command for `omni-dev gmail draft create` (#1923).

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::cli::gmail::format::{
    output_as, sanitize_for_terminal, write_scalar_jsonl, JsonlSerialize, OutputFormat,
};
use crate::cli::gmail::helpers::with_modify_scope_hint;
use crate::gmail::client::GmailClient;
use crate::gmail::compose::{
    check_subject, inline_image_warning, message_id_domain, plain_text_from_html,
    subject_matches_reply, Attachment, Composition, Mailbox, ReplyContext, REPLY_HEADERS,
};
use crate::gmail::drafts_api::DraftsApi;
use crate::gmail::messages_api::{
    ensure_within_message_limit, MessageFormat, MessagesApi, MAX_INSERT_BYTES,
};
use crate::gmail::profile_api::ProfileApi;
use crate::gmail::send_as_api::SendAsApi;

/// What `draft create` does with a message, for size-limit refusals.
const CREATE_ACTION: &str = "create a draft of";

/// Every composition flag, which `--raw` conflicts with.
const COMPOSE_ARGS: [&str; 11] = [
    "to",
    "cc",
    "bcc",
    "subject",
    "body",
    "body_file",
    "html_body",
    "html_body_file",
    "attach",
    "reply_to",
    "reply_all",
];

/// Creates a Gmail draft for a person to review and send from Gmail.
///
/// Builds a plain-text message from the flags, or with `--raw` uploads a
/// complete `.eml` file unchanged. `From` is left to Gmail, which fills in
/// the account's own address. The body comes from `--body`, `--body-file`,
/// or else standard input.
///
/// `--html-body` or `--html-body-file` adds an HTML version, sent as
/// `multipart/alternative` with a plain-text part. That part is `--body`
/// (or `--body-file`) when given, and otherwise the HTML converted to
/// Markdown; standard input is then never read. Inline images (`cid:`) are
/// not supported.
///
/// `--reply-to` takes the Gmail message id of the message being answered
/// (as `gmail search` and `gmail read` print it, not its `Message-ID`
/// header). The draft is filed into that message's thread, with
/// `In-Reply-To`/`References` set and the subject defaulting to
/// `Re: <original subject>`. Without `--to`, the draft goes to the
/// original's `Reply-To` (else its `From`, or its `To` when you sent it), and
/// `--reply-all` adds its other recipients, as a mail client's Reply and
/// Reply All do. An explicit `--to` or `--cc` replaces that header's default.
///
/// Needs the `gmail.modify` scope (`gmail auth login --modify`). Nothing is
/// ever sent: the draft waits in Gmail's Drafts folder.
#[derive(Parser)]
pub struct CreateCommand {
    /// A `To` recipient: `addr@example.com` or `"Name <addr@example.com>"`.
    /// Repeat the flag (or list several after it) for more recipients.
    /// Required unless `--reply-to` is given, which defaults it from the
    /// original.
    #[arg(long, value_name = "ADDR", num_args = 1.., required_unless_present_any = ["raw", "reply_to"])]
    pub to: Vec<String>,

    /// A `Cc` recipient, in the same form as `--to`.
    #[arg(long, value_name = "ADDR", num_args = 1..)]
    pub cc: Vec<String>,

    /// A `Bcc` recipient, in the same form as `--to`.
    #[arg(long, value_name = "ADDR", num_args = 1..)]
    pub bcc: Vec<String>,

    /// The subject. Required unless `--reply-to` is given, which defaults
    /// it to `Re: <original subject>`.
    #[arg(long, required_unless_present_any = ["raw", "reply_to"])]
    pub subject: Option<String>,

    /// The plain-text body.
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,

    /// Read the plain-text body from this UTF-8 file.
    #[arg(long, value_name = "PATH")]
    pub body_file: Option<PathBuf>,

    /// An HTML body, sent alongside the plain-text one. Without `--body` or
    /// `--body-file`, the plain text is derived from the HTML.
    #[arg(long, value_name = "HTML", conflicts_with = "html_body_file")]
    pub html_body: Option<String>,

    /// Read the HTML body from this UTF-8 file.
    #[arg(long, value_name = "PATH")]
    pub html_body_file: Option<PathBuf>,

    /// Attach a file. Repeat the flag (or list several after it) for more.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub attach: Vec<PathBuf>,

    /// Reply to this Gmail message id, filing the draft in its thread.
    /// Without `--to`, replies to the original's `Reply-To`, else its `From`
    /// (or, for a message you sent, its `To`).
    #[arg(long, value_name = "MESSAGE_ID")]
    pub reply_to: Option<String>,

    /// With `--reply-to`, also address the original's other `To` and `Cc`
    /// recipients, leaving out your own addresses. `--to`/`--cc` still
    /// replace the defaults for their header.
    #[arg(long, requires = "reply_to")]
    pub reply_all: bool,

    /// Upload this complete RFC 5322 message (`.eml`) byte for byte instead
    /// of composing one.
    #[arg(long, value_name = "FILE", conflicts_with_all = COMPOSE_ARGS)]
    pub raw: Option<PathBuf>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CreateCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    ///
    /// Reads every local input (the raw file, body, attachments) before any
    /// request, so a bad path or an oversize file fails without touching
    /// Gmail.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        let input = if let Some(path) = &self.raw {
            DraftInput::Raw(read_limited(path, CREATE_ACTION)?)
        } else {
            let html_body = resolve_html_body(
                self.html_body,
                self.html_body_file.as_deref(),
                CREATE_ACTION,
            )?;
            let stdin = std::io::stdin();
            let stdin_is_terminal = stdin.is_terminal();
            let body = resolve_body(
                self.body,
                self.body_file.as_deref(),
                html_body.as_deref(),
                stdin.lock(),
                stdin_is_terminal,
            )?;
            let body_len = body.len() + html_body.as_ref().map_or(0, String::len);
            let attachments = load_attachments(&self.attach, body_len)?;
            DraftInput::Compose(ComposeInput {
                to: self.to,
                cc: self.cc,
                bcc: self.bcc,
                subject: self.subject,
                body,
                html_body,
                attachments,
                reply_to: self.reply_to,
                reply_all: self.reply_all,
            })
        };
        let created = run_create(client, input).await?;
        if output_as(&created, &self.output)? {
            return Ok(());
        }
        render_created(&created, &mut std::io::stdout().lock())
    }
}

/// What to turn into a draft, with every local file already read.
#[derive(Debug)]
enum DraftInput {
    /// A complete message, uploaded unchanged.
    Raw(Vec<u8>),
    /// A message to build from its parts.
    Compose(ComposeInput),
}

/// The unparsed composition flags plus the resolved body and attachments.
#[derive(Debug, Default)]
struct ComposeInput {
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: Option<String>,
    body: String,
    html_body: Option<String>,
    attachments: Vec<Attachment>,
    reply_to: Option<String>,
    reply_all: bool,
}

/// The ids of a newly created draft.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct CreatedDraft {
    /// The draft id every drafts endpoint is addressed by.
    draft_id: String,
    /// The draft's current message id. It changes each time the draft is
    /// saved.
    message_id: String,
    /// The thread the draft belongs to.
    thread_id: String,
}

impl JsonlSerialize for CreatedDraft {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Builds (or takes) the message and creates the draft.
///
/// Split from [`CreateCommand::execute`] so tests can inject a wiremock
/// client and in-memory inputs.
async fn run_create(client: &GmailClient, input: DraftInput) -> Result<CreatedDraft> {
    let (raw, thread_id, notes) = match input {
        DraftInput::Raw(raw) => (raw, None, Vec::new()),
        DraftInput::Compose(compose) => {
            // Bad input fails here, before the lookups cost a request.
            let recipients = Recipients::parse(&compose)?;
            match &compose.subject {
                Some(subject) => check_subject(subject)?,
                None => ensure!(
                    compose.reply_to.is_some(),
                    "--subject is required unless --reply-to is given"
                ),
            }
            ensure!(
                !recipients.to.is_empty() || compose.reply_to.is_some(),
                "--to is required unless --reply-to is given"
            );
            ensure!(
                !compose.reply_all || compose.reply_to.is_some(),
                "--reply-all needs --reply-to"
            );
            // The profile gives the primary address; only a reply-all that
            // still has a header to fill also needs the send-as aliases.
            let wants_aliases =
                compose.reply_all && (recipients.to.is_empty() || recipients.cc.is_empty());
            // The lookups are independent, so they share a round trip.
            let (reply, account, aliases) = tokio::join!(
                async {
                    match &compose.reply_to {
                        Some(id) => fetch_reply_context(client, id).await.map(Some),
                        None => Ok(None),
                    }
                },
                fetch_account_address(client),
                async {
                    if wants_aliases {
                        fetch_send_as_addresses(client).await
                    } else {
                        Ok(Vec::new())
                    }
                },
            );
            let reply = reply?;
            let aliases = aliases?;
            let thread_id = reply.as_ref().map(|reply| reply.thread_id.clone());
            let (domain, fallback_warning) = match account
                .clone()
                .and_then(|email| message_id_domain_for(&email))
            {
                Ok(domain) => (Some(domain), None),
                Err(warning) => (None, Some(warning)),
            };
            let own: Vec<String> = account.into_iter().chain(aliases).collect();
            let (recipients, mut notes) = match &reply {
                Some(reply) => recipients.fill_from_reply(reply, compose.reply_all, &own)?,
                None => (recipients, Vec::new()),
            };
            notes.extend(fallback_warning.map(|warning| format!("warning: {warning}")));
            notes.extend(
                compose
                    .html_body
                    .as_deref()
                    .and_then(inline_image_warning)
                    .map(|warning| format!("warning: {warning}")),
            );
            (
                compose_message(compose, recipients, reply, domain)?,
                thread_id,
                notes,
            )
        }
    };
    let draft = DraftsApi::new(client)
        .create(&raw, thread_id.as_deref())
        .await
        .map_err(with_modify_scope_hint)?;
    // Only once the draft exists: a lookup that failed for the same reason
    // as the create would otherwise warn about a draft that never was, and
    // a note would describe the recipients of a draft that never was.
    for note in notes {
        eprintln!("{}", sanitize_for_terminal(&note));
    }
    Ok(CreatedDraft {
        draft_id: draft.id,
        message_id: draft.message.id,
        thread_id: draft.message.thread_id,
    })
}

/// Fetches the message being replied to (`format=metadata`, which
/// `gmail.readonly` allows) and reads its threading headers.
async fn fetch_reply_context(client: &GmailClient, message_id: &str) -> Result<ReplyContext> {
    let original = MessagesApi::new(client)
        .get(message_id, MessageFormat::Metadata, &REPLY_HEADERS)
        .await
        .with_context(|| format!("Failed to fetch message {message_id} to reply to"))?;
    ReplyContext::from_message(&original)
}

/// The account's primary address: the source of the draft's `Message-ID`
/// domain (#1953), and an address a reply leaves out (#1954).
///
/// Asks `users.getProfile` (which `gmail.readonly` allows) rather than the
/// cached `email_address` in settings, which is display-only and missing
/// for accounts set up before it existed. Never fails the command: the
/// `Err` is a warning for the caller to print, and the draft keeps
/// `mail-builder`'s `@localhost` id.
async fn fetch_account_address(client: &GmailClient) -> std::result::Result<String, String> {
    ProfileApi::new(client)
        .get()
        .await
        .map(|profile| profile.email_address)
        .map_err(|err| {
            format!(
                "could not look up the account's address ({err:#}); the draft's Message-ID \
                 ends in @localhost."
            )
        })
}

/// The `Message-ID` domain for the account address `email`, or the warning
/// to print when it has none.
fn message_id_domain_for(email: &str) -> std::result::Result<String, String> {
    message_id_domain(email).ok_or_else(|| {
        format!(
            "the account's address {email:?} has no usable domain; the draft's Message-ID \
             ends in @localhost."
        )
    })
}

/// The account's send-as addresses (the primary one and every alias), for
/// `--reply-all` to leave out (#1954).
///
/// Unlike [`fetch_account_address`], a failure fails the command: carrying
/// on would quietly put an alias in its own reply.
async fn fetch_send_as_addresses(client: &GmailClient) -> Result<Vec<String>> {
    let send_as = SendAsApi::new(client)
        .list()
        .await
        .context("Failed to list the account's send-as addresses for --reply-all")?;
    Ok(send_as
        .into_iter()
        .map(|alias| alias.send_as_email)
        .collect())
}

/// The parsed `--to`/`--cc`/`--bcc` values.
#[derive(Debug)]
struct Recipients {
    to: Vec<Mailbox>,
    cc: Vec<Mailbox>,
    bcc: Vec<Mailbox>,
}

impl Recipients {
    fn parse(input: &ComposeInput) -> Result<Self> {
        let parse_all = |values: &[String]| -> Result<Vec<Mailbox>> {
            values.iter().map(|value| Mailbox::parse(value)).collect()
        };
        Ok(Self {
            to: parse_all(&input.to)?,
            cc: parse_all(&input.cc)?,
            bcc: parse_all(&input.bcc)?,
        })
    }

    /// Fills an empty `To` (and, with `reply_all`, an empty `Cc`) from the
    /// message being replied to (see [`ReplyContext::default_recipients`]).
    ///
    /// A defaulted header leaves out `own` and every address already given
    /// explicitly, so nobody is named twice. When that leaves `To` empty, a
    /// defaulted `Cc` moves up into it, as mail clients do. When the draft
    /// would have no recipient at all, the reply goes back to the account
    /// itself (a note to self, as in Gmail), and failing that the command
    /// errors.
    ///
    /// Returns the recipients and the `warning:`/`note:` lines for the
    /// caller to show once the draft exists: unusable addresses in the
    /// headers that were consulted, and who was defaulted.
    fn fill_from_reply(
        mut self,
        reply: &ReplyContext,
        reply_all: bool,
        own: &[String],
    ) -> Result<(Self, Vec<String>)> {
        let default_to = self.to.is_empty();
        let default_cc = reply_all && self.cc.is_empty();
        if !default_to && !default_cc {
            return Ok((self, Vec::new()));
        }
        let named: Vec<String> = self
            .to
            .iter()
            .chain(&self.cc)
            .chain(&self.bcc)
            .map(|mailbox| mailbox.email.clone())
            .collect();
        let exclude: Vec<String> = own.iter().cloned().chain(named.iter().cloned()).collect();
        let (mut to, mut cc) = reply.default_recipients(reply_all, &exclude);
        if default_to && to.is_empty() && cc.is_empty() && named.is_empty() {
            // A note to self goes back to its primary target only: reply-all's
            // extras are all the account's own addresses here, and must stay
            // filtered.
            (to, cc) = reply.default_recipients(false, &[]);
        }
        if default_to && to.is_empty() && default_cc {
            to = std::mem::take(&mut cc);
        }

        let consulted = |header: &str| match header {
            "From" | "Reply-To" => default_to && !reply.sent_by_me,
            "To" => default_to && (reply.sent_by_me || reply_all),
            "Cc" => default_cc,
            _ => false,
        };
        let skipped: Vec<&str> = reply
            .skipped
            .iter()
            .filter(|(header, _)| consulted(header))
            .map(|(_, address)| address.as_str())
            .collect();
        ensure!(
            !(default_to
                && to.is_empty()
                && self.cc.is_empty()
                && cc.is_empty()
                && self.bcc.is_empty()),
            "found nobody to reply to: the original has no usable From/Reply-To (or To, for a \
             message you sent) that isn't already named{}; pass --to",
            if skipped.is_empty() {
                String::new()
            } else {
                format!(
                    " (skipped unusable {})",
                    sanitize_for_terminal(&skipped.join(", "))
                )
            }
        );

        let list = |mailboxes: &[Mailbox]| {
            mailboxes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut lines: Vec<String> = skipped
            .iter()
            .map(|address| {
                format!("warning: skipped an unusable address in the original: {address}")
            })
            .collect();
        let mut note = Vec::new();
        if default_to && !to.is_empty() {
            note.push(format!("replying to {}", list(&to)));
            self.to = to;
        }
        if default_cc && !cc.is_empty() {
            note.push(format!("cc {}", list(&cc)));
            self.cc = cc;
        }
        if !note.is_empty() {
            lines.push(format!("note: {}", note.join("; ")));
        }
        Ok((self, lines))
    }
}

/// Settles the subject and builds the message bytes.
///
/// When replying with an explicit subject that doesn't match the
/// original's, warns on stderr: Gmail starts a new thread when the subjects
/// differ.
fn compose_message(
    input: ComposeInput,
    recipients: Recipients,
    reply: Option<ReplyContext>,
    message_id_domain: Option<String>,
) -> Result<Vec<u8>> {
    let subject = match (input.subject, &reply) {
        (Some(subject), Some(reply)) => {
            if !subject_matches_reply(&subject, reply) {
                eprintln!(
                    "warning: the subject differs from {:?}; Gmail may file this draft in a new \
                     thread instead of the original's.",
                    reply.reply_subject()
                );
            }
            subject
        }
        (Some(subject), None) => subject,
        (None, Some(reply)) => reply.reply_subject(),
        (None, None) => bail!("--subject is required unless --reply-to is given"),
    };
    Composition {
        to: recipients.to,
        cc: recipients.cc,
        bcc: recipients.bcc,
        subject,
        body: input.body,
        html_body: input.html_body,
        attachments: input.attachments,
        reply,
        message_id_domain,
    }
    .build()
}

/// Picks the plain-text body from `--body`, then `--body-file`, then (with
/// an HTML body) the HTML converted by [`plain_alternative`], then `stdin`.
///
/// Stdin is never read when there is an HTML body: a script passing only
/// `--html-body-file` with an open stdin would otherwise hang, or send
/// whatever arrived there as the plain-text part. Refuses to read a
/// terminal: with no body flag and no piped input, the command would
/// otherwise sit waiting for typed input. Reads at most [`MAX_INSERT_BYTES`]
/// from a file or stdin, refusing anything larger.
fn resolve_body(
    body: Option<String>,
    body_file: Option<&Path>,
    html_body: Option<&str>,
    stdin: impl Read,
    stdin_is_terminal: bool,
) -> Result<String> {
    if let Some(body) = body {
        return Ok(body);
    }
    if let Some(path) = body_file {
        return read_text_file(path, CREATE_ACTION, "body");
    }
    if let Some(html) = html_body {
        return plain_alternative(None, html);
    }
    ensure!(
        !stdin_is_terminal,
        "no body given: pass --body TEXT, --body-file PATH or --html-body[-file], or pipe the \
         body on stdin"
    );
    let mut bytes = Vec::new();
    stdin
        .take(MAX_INSERT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("Failed to read the body from stdin")?;
    ensure_within_message_limit(bytes.len(), CREATE_ACTION)?;
    String::from_utf8(bytes).context("The body is not valid UTF-8")
}

/// The HTML body from `--html-body`, else `--html-body-file`; `None` when
/// neither is given. `action` is as for [`read_limited`].
pub(super) fn resolve_html_body(
    html_body: Option<String>,
    html_body_file: Option<&Path>,
    action: &str,
) -> Result<Option<String>> {
    match (html_body, html_body_file) {
        (Some(html), _) => Ok(Some(html)),
        (None, Some(path)) => read_text_file(path, action, "HTML body").map(Some),
        (None, None) => Ok(None),
    }
}

/// The plain-text part to send beside the HTML body `html`: `body` when the
/// caller gave one (verbatim, with no check that the two agree), else
/// derived from the HTML.
pub(super) fn plain_alternative(body: Option<String>, html: &str) -> Result<String> {
    match body {
        Some(body) => Ok(body),
        None => plain_text_from_html(html)
            .context("pass the plain-text body with --body or --body-file as well"),
    }
}

/// Reads a UTF-8 file with [`read_limited`]. `what` names its contents
/// ("body", "HTML body") in errors.
pub(super) fn read_text_file(path: &Path, action: &str, what: &str) -> Result<String> {
    let bytes = read_limited(path, action)
        .with_context(|| format!("Failed to read {what} file {}", path.display()))?;
    String::from_utf8(bytes).with_context(|| format!("The {what} is not valid UTF-8"))
}

/// Reads each attachment, after checking that they fit in
/// [`MAX_INSERT_BYTES`] once base64-encoded alongside a `body_len`-byte body,
/// so an oversize file is refused unread.
///
/// Headers and MIME boundaries aren't counted, so a message right at the
/// limit can still pass this and be refused by `DraftsApi::create`'s exact
/// check, which also runs before any request.
pub(super) fn load_attachments(paths: &[PathBuf], body_len: usize) -> Result<Vec<Attachment>> {
    let mut total = body_len as u64;
    for path in paths {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("Failed to read attachment {}", path.display()))?;
        ensure!(
            metadata.is_file(),
            "attachment {} is not a regular file",
            path.display()
        );
        total = total.saturating_add(base64_len(metadata.len()));
    }
    ensure!(
        total <= MAX_INSERT_BYTES,
        "refusing to attach these files: with the body they encode to about {total} bytes \
         (limit: {MAX_INSERT_BYTES} bytes), over Gmail's documented per-message size limit"
    );
    paths
        .iter()
        .map(|path| {
            let data = std::fs::read(path)
                .with_context(|| format!("Failed to read attachment {}", path.display()))?;
            let filename = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .with_context(|| format!("attachment {} has no file name", path.display()))?;
            let content_type = mime_guess::from_path(path)
                .first_or_octet_stream()
                .essence_str()
                .to_string();
            Ok(Attachment {
                filename,
                content_type,
                data,
            })
        })
        .collect()
}

/// Size of `len` bytes once base64-encoded in 76-column lines with CRLF
/// line breaks, as MIME writes them.
fn base64_len(len: u64) -> u64 {
    let encoded = len.div_ceil(3).saturating_mul(4);
    encoded.saturating_add(encoded.div_ceil(76).saturating_mul(2))
}

/// Reads a file, refusing one over [`MAX_INSERT_BYTES`] before reading it.
/// `action` names what the file is for in that refusal ("create a draft
/// of").
pub(super) fn read_limited(path: &Path, action: &str) -> Result<Vec<u8>> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("Failed to read {}", path.display()))?
        .len();
    ensure_within_message_limit(usize::try_from(len).unwrap_or(usize::MAX), action)?;
    std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))
}

/// Prints the three ids as aligned `NAME  value` lines.
fn render_created(created: &CreatedDraft, out: &mut dyn Write) -> Result<()> {
    for (name, value) in [
        ("DRAFT_ID  ", &created.draft_id),
        ("MESSAGE_ID", &created.message_id),
        ("THREAD_ID ", &created.thread_id),
    ] {
        writeln!(out, "{name}  {}", sanitize_for_terminal(value))
            .context("Failed to write draft ids")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

    const CREATE_PATH: &str = "/upload/gmail/v1/users/me/drafts";

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::Modify,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> GmailClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;

        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    async fn mount_create(server: &wiremock::MockServer, thread_id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(CREATE_PATH))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "r-1",
                    "message": {"id": "m-new", "threadId": thread_id},
                })),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    const PROFILE_PATH: &str = "/gmail/v1/users/me/profile";

    async fn mount_profile(server: &wiremock::MockServer, email: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(PROFILE_PATH))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": email,
                    "messagesTotal": 1,
                    "threadsTotal": 1,
                    "historyId": "1",
                })),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    /// The upload's decoded `Message-ID` value, without angle brackets.
    async fn uploaded_message_id(server: &wiremock::MockServer) -> String {
        let body = create_request_body(server).await;
        let line = body
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Message-ID: "))
            .unwrap_or_else(|| panic!("no Message-ID in {body}"));
        line.trim_start_matches('<')
            .trim_end_matches('>')
            .to_string()
    }

    async fn create_request_body(server: &wiremock::MockServer) -> String {
        let request = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path() == CREATE_PATH)
            .unwrap();
        String::from_utf8(request.body).unwrap()
    }

    fn compose(to: &str, subject: Option<&str>) -> ComposeInput {
        ComposeInput {
            to: vec![to.to_string()],
            subject: subject.map(str::to_string),
            body: "Hi.".to_string(),
            ..ComposeInput::default()
        }
    }

    // ── run_create ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_create_builds_and_uploads_a_plain_draft() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "me@example.org").await;
        mount_create(&server, "t-new").await;

        let input = ComposeInput {
            cc: vec!["Carol <carol@example.com>".to_string()],
            bcc: vec!["dave@example.com".to_string()],
            ..compose("Alice <alice@example.com>", Some("Report"))
        };
        let created = run_create(&client, DraftInput::Compose(input))
            .await
            .unwrap();
        assert_eq!(
            created,
            CreatedDraft {
                draft_id: "r-1".to_string(),
                message_id: "m-new".to_string(),
                thread_id: "t-new".to_string(),
            }
        );

        let body = create_request_body(&server).await;
        assert!(body.contains("Subject: Report\r\n"), "{body}");
        // Exact address encoding is the builder's (see `gmail::compose`'s
        // tests); here it's enough that each header made it into the upload.
        assert!(body.contains("To: "), "{body}");
        assert!(body.contains("<alice@example.com>"), "{body}");
        assert!(body.contains("Cc: "), "{body}");
        assert!(body.contains("<carol@example.com>"), "{body}");
        assert!(body.contains("Bcc: <dave@example.com>\r\n"), "{body}");
        assert!(!body.contains("threadId"));
        assert!(!body.contains("In-Reply-To"));
        assert!(!body.contains("@localhost"), "{body}");
        assert!(
            uploaded_message_id(&server).await.ends_with("@example.org"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn run_create_falls_back_to_the_builders_id_when_the_profile_fails() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(PROFILE_PATH))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        mount_create(&server, "t-new").await;

        run_create(
            &client,
            DraftInput::Compose(compose("alice@example.com", Some("Hi"))),
        )
        .await
        .unwrap();
        assert!(uploaded_message_id(&server).await.ends_with("@localhost"));
    }

    #[tokio::test]
    async fn message_id_domain_for_rejects_an_address_without_a_usable_domain() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "me@[127.0.0.1]").await;

        let email = fetch_account_address(&client).await.unwrap();
        let warning = message_id_domain_for(&email).unwrap_err();
        assert!(warning.contains("no usable domain"), "{warning}");
    }

    #[tokio::test]
    async fn run_create_raw_never_asks_for_the_profile() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_create(&server, "t-new").await;

        run_create(
            &client,
            DraftInput::Raw(b"Subject: x\r\n\r\nx\r\n".to_vec()),
        )
        .await
        .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(requests.iter().all(|r| r.url.path() != PROFILE_PATH));
    }

    #[tokio::test]
    async fn run_create_reply_threads_the_draft_and_defaults_the_subject() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/m-orig",
            ))
            .and(wiremock::matchers::query_param("format", "metadata"))
            .and(wiremock::matchers::query_param(
                "metadataHeaders",
                "References",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m-orig",
                    "threadId": "t-orig",
                    "payload": {"headers": [
                        {"name": "Subject", "value": "Quarterly report"},
                        {"name": "Message-ID", "value": "<b@example.com>"},
                        {"name": "References", "value": "<a@example.com>"},
                    ]},
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_profile(&server, "me@example.org").await;
        mount_create(&server, "t-orig").await;

        let input = ComposeInput {
            reply_to: Some("m-orig".to_string()),
            ..compose("alice@example.com", None)
        };
        let created = run_create(&client, DraftInput::Compose(input))
            .await
            .unwrap();
        assert_eq!(created.thread_id, "t-orig");

        let body = create_request_body(&server).await;
        assert!(
            body.contains(r#"{"message":{"threadId":"t-orig"}}"#),
            "{body}"
        );
        assert!(body.contains("Subject: Re: Quarterly report\r\n"), "{body}");
        assert!(body.contains("In-Reply-To: <b@example.com>\r\n"), "{body}");
        assert!(
            body.contains("References: <a@example.com> <b@example.com>\r\n"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn run_create_reply_to_an_unknown_message_fails_before_creating() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/messages/nope"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        mount_profile(&server, "me@example.org").await;
        wiremock::Mock::given(wiremock::matchers::path(CREATE_PATH))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let input = ComposeInput {
            reply_to: Some("nope".to_string()),
            ..compose("alice@example.com", None)
        };
        let err = run_create(&client, DraftInput::Compose(input))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("to reply to"), "{err}");
    }

    #[tokio::test]
    async fn run_create_raw_uploads_the_file_bytes_unchanged() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_create(&server, "t-new").await;

        let raw = "From: me@example.com\nTo: a@example.com\nSubject: As-is\n\nUntouched LF body.\n";
        run_create(&client, DraftInput::Raw(raw.as_bytes().to_vec()))
            .await
            .unwrap();

        let body = create_request_body(&server).await;
        assert!(body.contains(raw), "{body}");
    }

    // ── CreateCommand::execute ──────────────────────────────────────
    //
    // These exercise `execute` itself (not just `run_create`), so a
    // `--body`/`--raw` flag is always given: reading real stdin would make
    // the test process- and environment-dependent.

    fn create_command(raw: Option<PathBuf>, output: OutputFormat) -> CreateCommand {
        CreateCommand {
            to: if raw.is_some() {
                vec![]
            } else {
                vec!["alice@example.com".to_string()]
            },
            cc: vec![],
            bcc: vec![],
            subject: if raw.is_some() {
                None
            } else {
                Some("Report".to_string())
            },
            body: if raw.is_some() {
                None
            } else {
                Some("Hi.".to_string())
            },
            body_file: None,
            html_body: None,
            html_body_file: None,
            attach: vec![],
            reply_to: None,
            reply_all: false,
            raw,
            output,
        }
    }

    #[tokio::test]
    async fn execute_counts_the_html_body_toward_the_attachment_limit() {
        // No mocks: the refusal must come before any request.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"abc").unwrap();

        let html = "x".repeat(usize::try_from(MAX_INSERT_BYTES).unwrap() - 8);
        // The HTML and the attachment fit on their own; the plain-text part
        // derived from the HTML must be counted too.
        let err = CreateCommand {
            html_body: Some(html),
            attach: vec![path],
            body: None,
            ..create_command(None, OutputFormat::Table)
        }
        .execute(&client)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("refusing to attach"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn execute_reports_an_unreadable_html_body_file_before_any_request() {
        // No mocks: the refusal must come before any request.
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let err = CreateCommand {
            html_body_file: Some(dir.path().join("missing.html")),
            ..create_command(None, OutputFormat::Table)
        }
        .execute(&client)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("HTML body file"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_create_uploads_a_multipart_alternative_draft() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "me@example.org").await;
        mount_create(&server, "t-new").await;

        let input = ComposeInput {
            html_body: Some("<p>Hi <b>there</b>.</p>".to_string()),
            ..compose("alice@example.com", Some("Styled"))
        };
        run_create(&client, DraftInput::Compose(input))
            .await
            .unwrap();

        let body = create_request_body(&server).await;
        let message = body
            .split_once("Content-Type: message/rfc822\r\n\r\n")
            .unwrap_or_else(|| panic!("no message part in {body}"))
            .1;
        let parsed = mail_parser::MessageParser::default()
            .parse(message.as_bytes())
            .unwrap();
        assert_eq!(parsed.body_text(0).as_deref(), Some("Hi."));
        assert_eq!(
            parsed.body_html(0).as_deref(),
            Some("<p>Hi <b>there</b>.</p>")
        );
    }

    #[tokio::test]
    async fn execute_raw_path_uploads_the_file_and_renders_a_table() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_create(&server, "t-raw").await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.eml");
        std::fs::write(&path, b"To: a@example.com\r\nSubject: Raw\r\n\r\nBody.\r\n").unwrap();

        create_command(Some(path), OutputFormat::Table)
            .execute(&client)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_composes_from_flags_and_renders_jsonl() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "me@example.org").await;
        mount_create(&server, "t-flags").await;

        create_command(None, OutputFormat::Jsonl)
            .execute(&client)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_create_rejects_a_bad_recipient_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::path(CREATE_PATH))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let err = run_create(
            &client,
            DraftInput::Compose(compose("not-an-address", Some("Hi"))),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("invalid address"), "{err}");
    }

    #[tokio::test]
    async fn run_create_rejects_a_bad_subject_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::path_regex("^/(gmail|upload)/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        for (subject, expected) in [
            (None, "--subject is required"),
            (Some("Hi\r\nBcc: eve@example.com"), "line break"),
        ] {
            let err = run_create(
                &client,
                DraftInput::Compose(compose("alice@example.com", subject)),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
        }
    }

    #[tokio::test]
    async fn run_create_rejects_a_bad_recipient_before_the_reply_lookup() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::path_regex("^/(gmail|upload)/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let input = ComposeInput {
            cc: vec!["broken".to_string()],
            reply_to: Some("m-orig".to_string()),
            ..compose("alice@example.com", None)
        };
        let err = run_create(&client, DraftInput::Compose(input))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid address"), "{err}");
    }

    #[tokio::test]
    async fn run_create_turns_a_read_only_403_into_an_actionable_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "me@example.org").await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(CREATE_PATH))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Request had insufficient authentication scopes.",
                        "errors": [{"reason": "insufficientPermissions"}],
                    }
                })),
            )
            .mount(&server)
            .await;

        let err = run_create(
            &client,
            DraftInput::Compose(compose("alice@example.com", Some("Hi"))),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("gmail auth login --modify"),
            "{err}"
        );
    }

    // ── Reply recipients (#1954) ─────────────────────────────────────

    const SEND_AS_PATH: &str = "/gmail/v1/users/me/settings/sendAs";

    /// Mounts `m-orig` with the given address headers and labels.
    async fn mount_original(
        server: &wiremock::MockServer,
        headers: &[(&str, &str)],
        labels: &[&str],
    ) {
        let mut all = vec![
            serde_json::json!({"name": "Subject", "value": "Plans"}),
            serde_json::json!({"name": "Message-ID", "value": "<o@example.com>"}),
        ];
        all.extend(
            headers
                .iter()
                .map(|(name, value)| serde_json::json!({"name": name, "value": value})),
        );
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/m-orig",
            ))
            .and(wiremock::matchers::query_param(
                "metadataHeaders",
                "Reply-To",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "m-orig",
                    "threadId": "t-orig",
                    "labelIds": labels,
                    "payload": {"headers": all},
                })),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_send_as(server: &wiremock::MockServer, expected: u64) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "sendAs": [
                        {"sendAsEmail": "me@example.org", "isPrimary": true},
                        {"sendAsEmail": "alias@example.net"},
                    ],
                })),
            )
            .expect(expected)
            .mount(server)
            .await;
    }

    async fn mount_no_create(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::path(CREATE_PATH))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(server)
            .await;
    }

    fn reply_input(reply_all: bool) -> ComposeInput {
        ComposeInput {
            body: "Hi.".to_string(),
            reply_to: Some("m-orig".to_string()),
            reply_all,
            ..ComposeInput::default()
        }
    }

    /// The addresses in the upload's `name` header, in order.
    async fn uploaded(server: &wiremock::MockServer, name: &str) -> Vec<String> {
        let body = create_request_body(server).await;
        // The message is the upload's `message/rfc822` part.
        let marker = "Content-Type: message/rfc822\r\n\r\n";
        let message = &body[body.find(marker).unwrap() + marker.len()..];
        let parsed = mail_parser::MessageParser::default()
            .parse(message.as_bytes())
            .unwrap();
        let address = match name {
            "To" => parsed.to(),
            "Cc" => parsed.cc(),
            _ => parsed.bcc(),
        };
        address
            .map(|list| {
                list.iter()
                    .filter_map(|addr| addr.address().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn run_create_reply_without_to_replies_to_the_sender() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[
                ("From", "Alice <alice@example.com>"),
                ("To", "me@example.org, bob@example.com"),
                ("Cc", "carol@example.com"),
            ],
            &["INBOX"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_send_as(&server, 0).await;
        mount_create(&server, "t-orig").await;

        run_create(&client, DraftInput::Compose(reply_input(false)))
            .await
            .unwrap();
        assert_eq!(uploaded(&server, "To").await, ["alice@example.com"]);
        assert!(uploaded(&server, "Cc").await.is_empty());
    }

    #[tokio::test]
    async fn run_create_reply_prefers_reply_to() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[
                ("From", "alice@example.com"),
                ("Reply-To", "=?UTF-8?Q?Zo=C3=AB?= <list@example.com>"),
            ],
            &["INBOX"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_create(&server, "t-orig").await;

        run_create(&client, DraftInput::Compose(reply_input(false)))
            .await
            .unwrap();
        assert_eq!(uploaded(&server, "To").await, ["list@example.com"]);
    }

    #[tokio::test]
    async fn run_create_reply_all_addresses_everyone_but_me() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[
                ("From", "alice@example.com"),
                ("To", "Me <ME@example.org>, bob@example.com"),
                (
                    "Cc",
                    "alias@example.net, carol@example.com, Bob <bob@example.com>",
                ),
            ],
            &["INBOX"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_send_as(&server, 1).await;
        mount_create(&server, "t-orig").await;

        run_create(&client, DraftInput::Compose(reply_input(true)))
            .await
            .unwrap();
        assert_eq!(
            uploaded(&server, "To").await,
            ["alice@example.com", "bob@example.com"]
        );
        assert_eq!(uploaded(&server, "Cc").await, ["carol@example.com"]);
    }

    #[tokio::test]
    async fn run_create_reply_all_to_my_own_message_keeps_its_to() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[
                ("From", "alias@example.net"),
                ("To", "bob@example.com"),
                ("Cc", "carol@example.com"),
            ],
            &["SENT"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_send_as(&server, 1).await;
        mount_create(&server, "t-orig").await;

        run_create(&client, DraftInput::Compose(reply_input(true)))
            .await
            .unwrap();
        assert_eq!(uploaded(&server, "To").await, ["bob@example.com"]);
        assert_eq!(uploaded(&server, "Cc").await, ["carol@example.com"]);
    }

    #[tokio::test]
    async fn run_create_plain_reply_leaves_out_the_profile_address() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[
                ("From", "me@example.org"),
                ("To", "Me@Example.org, bob@example.com"),
            ],
            &["SENT"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_send_as(&server, 0).await;
        mount_create(&server, "t-orig").await;

        run_create(&client, DraftInput::Compose(reply_input(false)))
            .await
            .unwrap();
        assert_eq!(uploaded(&server, "To").await, ["bob@example.com"]);
    }

    #[tokio::test]
    async fn run_create_explicit_to_and_cc_skip_the_send_as_lookup() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[("From", "alice@example.com"), ("Cc", "carol@example.com")],
            &["INBOX"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_send_as(&server, 0).await;
        mount_create(&server, "t-orig").await;

        let input = ComposeInput {
            to: vec!["dan@example.com".to_string()],
            cc: vec!["erin@example.com".to_string()],
            ..reply_input(true)
        };
        run_create(&client, DraftInput::Compose(input))
            .await
            .unwrap();
        assert_eq!(uploaded(&server, "To").await, ["dan@example.com"]);
        assert_eq!(uploaded(&server, "Cc").await, ["erin@example.com"]);
    }

    #[tokio::test]
    async fn run_create_explicit_recipients_are_not_defaulted_again() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[
                ("From", "alice@example.com"),
                ("To", "bob@example.com"),
                ("Cc", "carol@example.com"),
            ],
            &["INBOX"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_send_as(&server, 1).await;
        mount_create(&server, "t-orig").await;

        // Bob moved to Cc, Carol to Bcc: each appears only where named.
        let input = ComposeInput {
            cc: vec!["bob@example.com".to_string()],
            bcc: vec!["carol@example.com".to_string()],
            ..reply_input(true)
        };
        run_create(&client, DraftInput::Compose(input))
            .await
            .unwrap();
        assert_eq!(uploaded(&server, "To").await, ["alice@example.com"]);
        assert_eq!(uploaded(&server, "Cc").await, ["bob@example.com"]);
        assert_eq!(uploaded(&server, "Bcc").await, ["carol@example.com"]);
    }

    #[tokio::test]
    async fn run_create_reply_all_fails_when_send_as_fails() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(&server, &[("From", "alice@example.com")], &["INBOX"]).await;
        mount_profile(&server, "me@example.org").await;
        wiremock::Mock::given(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        mount_no_create(&server).await;

        let err = run_create(&client, DraftInput::Compose(reply_input(true)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("send-as addresses"), "{err}");
    }

    #[tokio::test]
    async fn run_create_reply_with_nobody_to_reply_to_fails_before_creating() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_original(
            &server,
            &[
                ("From", "me@example.org"),
                ("To", "undisclosed-recipients:;"),
            ],
            &["SENT"],
        )
        .await;
        mount_profile(&server, "me@example.org").await;
        mount_no_create(&server).await;

        let err = run_create(&client, DraftInput::Compose(reply_input(false)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pass --to"), "{err}");
    }

    #[tokio::test]
    async fn run_create_rejects_missing_to_or_reply_to_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::path_regex("^/(gmail|upload)/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        for (input, expected) in [
            (
                ComposeInput {
                    to: Vec::new(),
                    ..compose("unused@example.com", Some("Hi"))
                },
                "--to is required",
            ),
            (
                ComposeInput {
                    reply_all: true,
                    ..compose("alice@example.com", Some("Hi"))
                },
                "--reply-all needs --reply-to",
            ),
        ] {
            let err = run_create(&client, DraftInput::Compose(input))
                .await
                .unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
        }
    }

    fn parsed(values: &[&str]) -> Vec<Mailbox> {
        values.iter().map(|v| Mailbox::parse(v).unwrap()).collect()
    }

    fn recipients(to: &[&str], cc: &[&str]) -> Recipients {
        Recipients {
            to: parsed(to),
            cc: parsed(cc),
            bcc: Vec::new(),
        }
    }

    fn me() -> Vec<String> {
        vec!["ME@example.org".to_string()]
    }

    #[test]
    fn fill_from_reply_describes_what_it_defaulted() {
        let reply = ReplyContext {
            from: parsed(&["Alice <alice@example.com>"]),
            cc: parsed(&["bob@example.com"]),
            ..ReplyContext::default()
        };
        let (_, lines) = recipients(&[], &[])
            .fill_from_reply(&reply, true, &[])
            .unwrap();
        assert_eq!(
            lines,
            ["note: replying to Alice <alice@example.com>; cc bob@example.com"]
        );
        let (_, lines) = recipients(&[], &[])
            .fill_from_reply(&reply, false, &[])
            .unwrap();
        assert_eq!(lines, ["note: replying to Alice <alice@example.com>"]);
    }

    #[test]
    fn fill_from_reply_leaves_explicit_headers_alone() {
        let reply = ReplyContext {
            from: parsed(&["alice@example.com"]),
            cc: parsed(&["bob@example.com"]),
            ..ReplyContext::default()
        };
        let (filled, lines) = recipients(&["dan@example.com"], &[])
            .fill_from_reply(&reply, false, &[])
            .unwrap();
        assert_eq!(filled.to, parsed(&["dan@example.com"]));
        assert!(filled.cc.is_empty());
        assert!(lines.is_empty());
        // Reply-all with an explicit To still defaults Cc.
        let (filled, lines) = recipients(&["dan@example.com"], &[])
            .fill_from_reply(&reply, true, &[])
            .unwrap();
        assert_eq!(filled.to, parsed(&["dan@example.com"]));
        assert_eq!(filled.cc, parsed(&["bob@example.com"]));
        assert_eq!(lines, ["note: cc bob@example.com"]);
        // With nothing to add, there is nothing to say.
        let (_, lines) = recipients(&["dan@example.com"], &[])
            .fill_from_reply(&ReplyContext::default(), true, &[])
            .unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn fill_from_reply_leaves_me_out_of_a_plain_reply() {
        let reply = ReplyContext {
            sent_by_me: true,
            from: parsed(&["me@example.org"]),
            to: parsed(&["me@example.org", "bob@example.com"]),
            ..ReplyContext::default()
        };
        let (filled, _) = recipients(&[], &[])
            .fill_from_reply(&reply, false, &me())
            .unwrap();
        assert_eq!(filled.to, parsed(&["bob@example.com"]));
    }

    #[test]
    fn fill_from_reply_moves_cc_up_when_to_is_left_empty() {
        // I sent it to myself, copying Bob and Carol.
        let reply = ReplyContext {
            sent_by_me: true,
            to: parsed(&["me@example.org"]),
            cc: parsed(&["bob@example.com", "carol@example.com"]),
            ..ReplyContext::default()
        };
        let (filled, lines) = recipients(&[], &[])
            .fill_from_reply(&reply, true, &me())
            .unwrap();
        assert_eq!(filled.to, parsed(&["bob@example.com", "carol@example.com"]));
        assert!(filled.cc.is_empty());
        assert_eq!(
            lines,
            ["note: replying to bob@example.com, carol@example.com"]
        );
    }

    #[test]
    fn fill_from_reply_answers_a_note_to_self_to_myself() {
        let reply = ReplyContext {
            sent_by_me: true,
            from: parsed(&["me@example.org"]),
            to: parsed(&["me@example.org"]),
            ..ReplyContext::default()
        };
        for reply_all in [false, true] {
            let (filled, _) = recipients(&[], &[])
                .fill_from_reply(&reply, reply_all, &me())
                .unwrap();
            assert_eq!(filled.to, parsed(&["me@example.org"]));
        }
    }

    #[test]
    fn fill_from_reply_keeps_aliases_out_of_a_note_to_self() {
        let reply = ReplyContext {
            sent_by_me: true,
            from: parsed(&["me@example.org"]),
            to: parsed(&["me@example.org", "alias@example.net"]),
            cc: parsed(&["alias@example.net"]),
            ..ReplyContext::default()
        };
        let own = [
            "me@example.org".to_string(),
            "alias@example.net".to_string(),
        ];
        let (filled, _) = recipients(&[], &[])
            .fill_from_reply(&reply, true, &own)
            .unwrap();
        assert_eq!(filled.to, parsed(&["me@example.org", "alias@example.net"]));
        assert!(filled.cc.is_empty(), "{:?}", filled.cc);
    }

    #[test]
    fn fill_from_reply_allows_a_draft_with_only_an_explicit_cc() {
        // `--cc alice` to Alice's message: she is already named, so `To`
        // stays empty, but the draft still has a recipient.
        let reply = ReplyContext {
            from: parsed(&["alice@example.com"]),
            ..ReplyContext::default()
        };
        let (filled, lines) = recipients(&[], &["alice@example.com"])
            .fill_from_reply(&reply, false, &[])
            .unwrap();
        assert!(filled.to.is_empty());
        assert_eq!(filled.cc, parsed(&["alice@example.com"]));
        assert!(lines.is_empty());
    }

    #[test]
    fn fill_from_reply_refuses_a_draft_with_no_recipient() {
        let reply = ReplyContext {
            skipped: vec![("From", "eve@example.com".to_string())],
            ..ReplyContext::default()
        };
        let err = recipients(&[], &[])
            .fill_from_reply(&reply, true, &me())
            .unwrap_err()
            .to_string();
        assert!(err.contains("pass --to"), "{err}");
        assert!(err.contains("skipped unusable eve@example.com"), "{err}");
    }

    #[test]
    fn fill_from_reply_warns_only_about_headers_it_used() {
        let reply = ReplyContext {
            from: parsed(&["alice@example.com"]),
            skipped: vec![
                ("From", "bad-from@example.com".to_string()),
                ("To", "bad-to@example.com".to_string()),
                ("Cc", "bad-cc@example.com".to_string()),
                // `ReplyContext::from_message` never records a header
                // outside From/Reply-To/To/Cc, but `consulted`'s catch-all
                // must still never warn about one.
                ("Bcc", "bad-bcc@example.com".to_string()),
            ],
            ..ReplyContext::default()
        };
        let warnings = |recipients: Recipients, reply_all| -> Vec<String> {
            recipients
                .fill_from_reply(&reply, reply_all, &[])
                .unwrap()
                .1
                .into_iter()
                .filter(|line| line.starts_with("warning:"))
                .collect()
        };
        assert_eq!(
            warnings(recipients(&[], &[]), false),
            ["warning: skipped an unusable address in the original: bad-from@example.com"]
        );
        assert_eq!(warnings(recipients(&[], &[]), true).len(), 3);
        // Only Cc is defaulted here.
        assert_eq!(
            warnings(recipients(&["dan@example.com"], &[]), true),
            ["warning: skipped an unusable address in the original: bad-cc@example.com"]
        );
    }

    // ── compose_message ──────────────────────────────────────────────

    #[test]
    fn compose_message_keeps_an_explicit_reply_subject() {
        let reply = ReplyContext {
            thread_id: "t".to_string(),
            subject: "Report".to_string(),
            ..ReplyContext::default()
        };
        let input = compose("a@example.com", Some("Different"));
        let recipients = Recipients::parse(&input).unwrap();
        let message = compose_message(input, recipients, Some(reply), None).unwrap();
        assert!(String::from_utf8(message)
            .unwrap()
            .contains("Subject: Different\r\n"));
    }

    #[test]
    fn compose_message_requires_a_subject_without_a_reply() {
        let input = compose("a@example.com", None);
        let recipients = Recipients::parse(&input).unwrap();
        let err = compose_message(input, recipients, None, None).unwrap_err();
        assert!(err.to_string().contains("--subject"), "{err}");
    }

    // ── resolve_body ─────────────────────────────────────────────────

    #[test]
    fn resolve_body_prefers_the_flag() {
        let body =
            resolve_body(Some("flag".to_string()), None, None, &b"stdin"[..], false).unwrap();
        assert_eq!(body, "flag");
    }

    #[test]
    fn resolve_body_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.txt");
        std::fs::write(&path, "from file\n").unwrap();
        let body = resolve_body(None, Some(&path), None, &b"stdin"[..], false).unwrap();
        assert_eq!(body, "from file\n");
    }

    #[test]
    fn resolve_body_reports_a_missing_file() {
        let err = resolve_body(
            None,
            Some(Path::new("/nonexistent/b.txt")),
            None,
            &b""[..],
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("body file"), "{err}");
    }

    #[test]
    fn resolve_body_falls_back_to_piped_stdin() {
        let body = resolve_body(None, None, None, &b"piped"[..], false).unwrap();
        assert_eq!(body, "piped");
    }

    #[test]
    fn resolve_body_refuses_oversize_stdin_after_a_bounded_read() {
        let limit = usize::try_from(MAX_INSERT_BYTES).unwrap();
        let big = std::io::repeat(b'a').take(MAX_INSERT_BYTES + 10);
        let err = resolve_body(None, None, None, big, false).unwrap_err();
        assert!(
            err.to_string().contains("refusing to create a draft of"),
            "{err}"
        );
        let at_limit = std::io::repeat(b'a').take(MAX_INSERT_BYTES);
        assert_eq!(
            resolve_body(None, None, None, at_limit, false)
                .unwrap()
                .len(),
            limit
        );
    }

    #[test]
    fn resolve_body_refuses_an_oversize_file_unread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_INSERT_BYTES + 1)
            .unwrap();
        let err = resolve_body(None, Some(&path), None, &b""[..], false).unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to create a draft of"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_body_rejects_non_utf8() {
        let err = resolve_body(None, None, None, &[0xffu8, 0xfe][..], false).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    /// A stdin that fails the test if anything reads it.
    struct UnreadableStdin;

    impl Read for UnreadableStdin {
        // omni-dev: coverage ignore reason="never called: every test using UnreadableStdin gives an HTML body, so resolve_body returns before reading stdin; the panic exists to fail loudly if that ever changes"
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("stdin was read")
        }
        // omni-dev: coverage end
    }

    #[test]
    fn resolve_body_derives_the_plain_text_from_html_without_reading_stdin() {
        let html = "<p>See <a href=\"https://example.com\">this</a>.</p>";
        // A terminal, too: with an HTML body there is nothing to wait for.
        let body = resolve_body(None, None, Some(html), UnreadableStdin, true).unwrap();
        assert_eq!(body, "See [this](https://example.com).");
    }

    #[test]
    fn resolve_body_keeps_an_explicit_body_beside_html() {
        let body = resolve_body(
            Some("Plain.".to_string()),
            None,
            Some("<p>Rich.</p>"),
            UnreadableStdin,
            false,
        )
        .unwrap();
        assert_eq!(body, "Plain.");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.txt");
        std::fs::write(&path, "From a file.").unwrap();
        let body = resolve_body(
            None,
            Some(&path),
            Some("<p>Rich.</p>"),
            UnreadableStdin,
            false,
        )
        .unwrap();
        assert_eq!(body, "From a file.");
    }

    #[test]
    fn resolve_html_body_prefers_the_flag_then_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.html");
        std::fs::write(&path, "<p>From file</p>").unwrap();
        assert_eq!(
            resolve_html_body(Some("<p>Flag</p>".to_string()), None, CREATE_ACTION).unwrap(),
            Some("<p>Flag</p>".to_string())
        );
        assert_eq!(
            resolve_html_body(None, Some(&path), CREATE_ACTION).unwrap(),
            Some("<p>From file</p>".to_string())
        );
        assert_eq!(resolve_html_body(None, None, CREATE_ACTION).unwrap(), None);
    }

    #[test]
    fn resolve_html_body_rejects_a_missing_or_non_utf8_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_html_body(None, Some(&dir.path().join("missing.html")), CREATE_ACTION)
            .unwrap_err();
        assert!(err.to_string().contains("HTML body file"), "{err}");

        let path = dir.path().join("bad.html");
        std::fs::write(&path, [0xffu8, 0xfe]).unwrap();
        let err = resolve_html_body(None, Some(&path), CREATE_ACTION).unwrap_err();
        assert!(
            err.to_string().contains("HTML body is not valid UTF-8"),
            "{err}"
        );
    }

    #[test]
    fn resolve_body_refuses_to_wait_on_a_terminal() {
        let err = resolve_body(None, None, None, &b""[..], true).unwrap_err();
        assert!(err.to_string().contains("no body given"), "{err}");
    }

    // ── load_attachments / read_limited ──────────────────────────────

    #[test]
    fn load_attachments_reads_name_type_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("report.pdf");
        let unknown = dir.path().join("blob.zzunknown");
        std::fs::write(&pdf, b"%PDF").unwrap();
        std::fs::write(&unknown, [0u8, 1, 2]).unwrap();

        let attachments = load_attachments(&[pdf, unknown], 0).unwrap();
        assert_eq!(
            attachments,
            [
                Attachment {
                    filename: "report.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    data: b"%PDF".to_vec(),
                },
                Attachment {
                    filename: "blob.zzunknown".to_string(),
                    content_type: "application/octet-stream".to_string(),
                    data: vec![0, 1, 2],
                },
            ]
        );
    }

    #[test]
    fn load_attachments_rejects_missing_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_attachments(&[dir.path().join("missing")], 0).is_err());
        let err = load_attachments(&[dir.path().to_path_buf()], 0).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn load_attachments_refuses_a_file_that_only_base64_pushes_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // Under the limit on disk, over it once encoded. Sparse: the check
        // reads metadata only, never the contents.
        let on_disk = MAX_INSERT_BYTES * 4 / 5;
        std::fs::File::create(&path)
            .unwrap()
            .set_len(on_disk)
            .unwrap();
        assert!(on_disk < MAX_INSERT_BYTES && base64_len(on_disk) > MAX_INSERT_BYTES);
        let err = load_attachments(&[path], 0).unwrap_err();
        assert!(err.to_string().contains("refusing to attach"), "{err}");
    }

    #[test]
    fn load_attachments_counts_the_body_toward_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"abc").unwrap();
        let body_len = usize::try_from(MAX_INSERT_BYTES).unwrap() - 2;
        assert!(load_attachments(std::slice::from_ref(&path), 0).is_ok());
        assert!(load_attachments(&[path], body_len).is_err());
    }

    #[test]
    fn base64_len_matches_mime_line_wrapping() {
        assert_eq!(base64_len(0), 0);
        assert_eq!(base64_len(1), 4 + 2);
        assert_eq!(base64_len(57), 76 + 2);
        assert_eq!(base64_len(58), 80 + 4);
    }

    #[test]
    fn read_limited_refuses_an_oversize_raw_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.eml");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_INSERT_BYTES + 1)
            .unwrap();
        let err = read_limited(&path, CREATE_ACTION).unwrap_err();
        assert!(
            err.to_string().contains("refusing to create a draft"),
            "{err}"
        );
    }

    #[test]
    fn read_limited_returns_the_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.eml");
        std::fs::write(&path, b"Subject: x\n\nbody").unwrap();
        assert_eq!(
            read_limited(&path, CREATE_ACTION).unwrap(),
            b"Subject: x\n\nbody"
        );
    }

    // ── render_created ───────────────────────────────────────────────

    #[test]
    fn render_created_labels_all_three_ids() {
        let mut out = Vec::new();
        render_created(
            &CreatedDraft {
                draft_id: "r-1".to_string(),
                message_id: "m-1".to_string(),
                thread_id: "t-1".to_string(),
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "DRAFT_ID    r-1\nMESSAGE_ID  m-1\nTHREAD_ID   t-1\n"
        );
    }

    // ── clap surface ─────────────────────────────────────────────────

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        create: CreateCommand,
    }

    fn parse(args: &[&str]) -> Result<CreateCommand, clap::Error> {
        let argv = std::iter::once("create").chain(args.iter().copied());
        Harness::try_parse_from(argv).map(|h| h.create)
    }

    #[test]
    fn clap_requires_to_and_subject_when_composing() {
        assert!(parse(&["--subject", "s"]).is_err());
        assert!(parse(&["--to", "a@example.com"]).is_err());
        assert!(parse(&["--to", "a@example.com", "--subject", "s"]).is_ok());
    }

    #[test]
    fn clap_reply_to_makes_subject_optional() {
        assert!(parse(&["--to", "a@example.com", "--reply-to", "m1"]).is_ok());
    }

    #[test]
    fn clap_reply_to_makes_to_optional() {
        let cmd = parse(&["--reply-to", "m1"]).unwrap();
        assert!(cmd.to.is_empty() && !cmd.reply_all);
        assert!(
            parse(&["--reply-to", "m1", "--reply-all"])
                .unwrap()
                .reply_all
        );
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn clap_reply_all_requires_reply_to() {
        assert!(parse(&["--to", "a@example.com", "--subject", "s", "--reply-all"]).is_err());
    }

    #[test]
    fn clap_raw_stands_alone() {
        assert!(parse(&["--raw", "m.eml"]).is_ok());
        for flag in [
            &["--to", "a@example.com"][..],
            &["--subject", "s"],
            &["--body", "b"],
            &["--attach", "f"],
            &["--reply-to", "m1"],
            &["--reply-all"],
        ] {
            let mut args = vec!["--raw", "m.eml"];
            args.extend(flag);
            assert!(parse(&args).is_err(), "--raw accepted {flag:?}");
        }
    }

    #[test]
    fn clap_collects_repeated_and_multi_value_recipients() {
        let cmd = parse(&[
            "--to",
            "a@example.com",
            "b@example.com",
            "--to",
            "Doe, Jane <j@example.com>",
            "--subject",
            "s",
        ])
        .unwrap();
        assert_eq!(
            cmd.to,
            [
                "a@example.com",
                "b@example.com",
                "Doe, Jane <j@example.com>"
            ]
        );
    }

    #[test]
    fn clap_html_body_and_html_body_file_conflict() {
        let base = ["--to", "a@example.com", "--subject", "s"];
        let with = |extra: &[&'static str]| {
            let mut args = base.to_vec();
            args.extend(extra);
            parse(&args)
        };
        let cmd = with(&["--html-body", "<p>x</p>"]).unwrap();
        assert_eq!(cmd.html_body.as_deref(), Some("<p>x</p>"));
        assert!(with(&["--html-body-file", "x.html", "--body", "b"]).is_ok());
        assert!(with(&["--html-body", "<p>x</p>", "--html-body-file", "x.html"]).is_err());
        for flag in ["--html-body", "--html-body-file"] {
            assert!(
                parse(&["--raw", "m.eml", flag, "x"]).is_err(),
                "--raw accepted {flag}"
            );
        }
    }

    #[test]
    fn clap_body_and_body_file_conflict() {
        assert!(parse(&[
            "--to",
            "a@example.com",
            "--subject",
            "s",
            "--body",
            "b",
            "--body-file",
            "f"
        ])
        .is_err());
    }
}
