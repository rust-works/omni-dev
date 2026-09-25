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
    check_subject, message_id_domain, subject_matches_reply, Attachment, Composition, Mailbox,
    ReplyContext, REPLY_HEADERS,
};
use crate::gmail::drafts_api::DraftsApi;
use crate::gmail::messages_api::{
    ensure_within_message_limit, MessageFormat, MessagesApi, MAX_INSERT_BYTES,
};
use crate::gmail::profile_api::ProfileApi;

/// What `draft create` does with a message, for size-limit refusals.
const CREATE_ACTION: &str = "create a draft of";

/// Every composition flag, which `--raw` conflicts with.
const COMPOSE_ARGS: [&str; 8] = [
    "to",
    "cc",
    "bcc",
    "subject",
    "body",
    "body_file",
    "attach",
    "reply_to",
];

/// Creates a Gmail draft for a person to review and send from Gmail.
///
/// Builds a `text/plain` message from the flags, or with `--raw` uploads a
/// complete `.eml` file unchanged. `From` is left to Gmail, which fills in
/// the account's own address. The body comes from `--body`, `--body-file`,
/// or else standard input.
///
/// `--reply-to` takes the Gmail message id of the message being answered
/// (as `gmail search` and `gmail read` print it, not its `Message-ID`
/// header). The draft is filed into that message's thread, with
/// `In-Reply-To`/`References` set and the subject defaulting to
/// `Re: <original subject>`.
///
/// Needs the `gmail.modify` scope (`gmail auth login --modify`). Nothing is
/// ever sent: the draft waits in Gmail's Drafts folder.
#[derive(Parser)]
pub struct CreateCommand {
    /// A `To` recipient: `addr@example.com` or `"Name <addr@example.com>"`.
    /// Repeat the flag (or list several after it) for more recipients.
    #[arg(long, value_name = "ADDR", num_args = 1.., required_unless_present = "raw")]
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

    /// Attach a file. Repeat the flag (or list several after it) for more.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub attach: Vec<PathBuf>,

    /// Reply to this Gmail message id, filing the draft in its thread.
    #[arg(long, value_name = "MESSAGE_ID")]
    pub reply_to: Option<String>,

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
            let stdin = std::io::stdin();
            let stdin_is_terminal = stdin.is_terminal();
            let body = resolve_body(
                self.body,
                self.body_file.as_deref(),
                stdin.lock(),
                stdin_is_terminal,
            )?;
            let attachments = load_attachments(&self.attach, body.len())?;
            DraftInput::Compose(ComposeInput {
                to: self.to,
                cc: self.cc,
                bcc: self.bcc,
                subject: self.subject,
                body,
                attachments,
                reply_to: self.reply_to,
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
    attachments: Vec<Attachment>,
    reply_to: Option<String>,
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
    let (raw, thread_id, fallback_warning) = match input {
        DraftInput::Raw(raw) => (raw, None, None),
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
            // The two lookups are independent, so they share a round trip.
            let (reply, domain) = tokio::join!(
                async {
                    match &compose.reply_to {
                        Some(id) => fetch_reply_context(client, id).await.map(Some),
                        None => Ok(None),
                    }
                },
                fetch_message_id_domain(client),
            );
            let reply = reply?;
            let thread_id = reply.as_ref().map(|reply| reply.thread_id.clone());
            let (domain, fallback_warning) = match domain {
                Ok(domain) => (Some(domain), None),
                Err(warning) => (None, Some(warning)),
            };
            (
                compose_message(compose, recipients, reply, domain)?,
                thread_id,
                fallback_warning,
            )
        }
    };
    let draft = DraftsApi::new(client)
        .create(&raw, thread_id.as_deref())
        .await
        .map_err(with_modify_scope_hint)?;
    // Only once the draft exists: a lookup that failed for the same reason
    // as the create would otherwise warn about a draft that never was.
    if let Some(warning) = fallback_warning {
        eprintln!("warning: {warning}");
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

/// The account's domain, for the draft's `Message-ID` (#1953).
///
/// Asks `users.getProfile` (which `gmail.readonly` allows) rather than the
/// cached `email_address` in settings, which is display-only and missing
/// for accounts set up before it existed. Never fails the command: the
/// `Err` is a warning for the caller to print, and the draft keeps
/// `mail-builder`'s `@localhost` id.
async fn fetch_message_id_domain(client: &GmailClient) -> std::result::Result<String, String> {
    let email = ProfileApi::new(client)
        .get()
        .await
        .map_err(|err| {
            format!(
                "could not look up the account's address ({err:#}); the draft's Message-ID \
                 ends in @localhost."
            )
        })?
        .email_address;
    message_id_domain(&email).ok_or_else(|| {
        format!(
            "the account's address {email:?} has no usable domain; the draft's Message-ID \
             ends in @localhost."
        )
    })
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
        attachments: input.attachments,
        reply,
        message_id_domain,
    }
    .build()
}

/// Picks the body from `--body`, then `--body-file`, then `stdin`.
///
/// Refuses to read a terminal: with no body flag and no piped input, the
/// command would otherwise sit waiting for typed input. Reads at most
/// [`MAX_INSERT_BYTES`] from a file or stdin, refusing anything larger.
fn resolve_body(
    body: Option<String>,
    body_file: Option<&Path>,
    stdin: impl Read,
    stdin_is_terminal: bool,
) -> Result<String> {
    if let Some(body) = body {
        return Ok(body);
    }
    let bytes = if let Some(path) = body_file {
        read_limited(path, CREATE_ACTION)
            .with_context(|| format!("Failed to read body file {}", path.display()))?
    } else {
        ensure!(
            !stdin_is_terminal,
            "no body given: pass --body TEXT or --body-file PATH, or pipe the body on stdin"
        );
        let mut bytes = Vec::new();
        stdin
            .take(MAX_INSERT_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Failed to read the body from stdin")?;
        ensure_within_message_limit(bytes.len(), CREATE_ACTION)?;
        bytes
    };
    String::from_utf8(bytes).context("The body is not valid UTF-8")
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
    async fn fetch_message_id_domain_rejects_an_address_without_a_usable_domain() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_profile(&server, "me@[127.0.0.1]").await;

        let warning = fetch_message_id_domain(&client).await.unwrap_err();
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
            attach: vec![],
            reply_to: None,
            raw,
            output,
        }
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
        let body = resolve_body(Some("flag".to_string()), None, &b"stdin"[..], false).unwrap();
        assert_eq!(body, "flag");
    }

    #[test]
    fn resolve_body_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.txt");
        std::fs::write(&path, "from file\n").unwrap();
        let body = resolve_body(None, Some(&path), &b"stdin"[..], false).unwrap();
        assert_eq!(body, "from file\n");
    }

    #[test]
    fn resolve_body_reports_a_missing_file() {
        let err =
            resolve_body(None, Some(Path::new("/nonexistent/b.txt")), &b""[..], false).unwrap_err();
        assert!(err.to_string().contains("body file"), "{err}");
    }

    #[test]
    fn resolve_body_falls_back_to_piped_stdin() {
        let body = resolve_body(None, None, &b"piped"[..], false).unwrap();
        assert_eq!(body, "piped");
    }

    #[test]
    fn resolve_body_refuses_oversize_stdin_after_a_bounded_read() {
        let limit = usize::try_from(MAX_INSERT_BYTES).unwrap();
        let big = std::io::repeat(b'a').take(MAX_INSERT_BYTES + 10);
        let err = resolve_body(None, None, big, false).unwrap_err();
        assert!(
            err.to_string().contains("refusing to create a draft of"),
            "{err}"
        );
        let at_limit = std::io::repeat(b'a').take(MAX_INSERT_BYTES);
        assert_eq!(
            resolve_body(None, None, at_limit, false).unwrap().len(),
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
        let err = resolve_body(None, Some(&path), &b""[..], false).unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to create a draft of"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_body_rejects_non_utf8() {
        let err = resolve_body(None, None, &[0xffu8, 0xfe][..], false).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn resolve_body_refuses_to_wait_on_a_terminal() {
        let err = resolve_body(None, None, &b""[..], true).unwrap_err();
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
    fn clap_raw_stands_alone() {
        assert!(parse(&["--raw", "m.eml"]).is_ok());
        for flag in [
            ["--to", "a@example.com"],
            ["--subject", "s"],
            ["--body", "b"],
            ["--attach", "f"],
            ["--reply-to", "m1"],
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
