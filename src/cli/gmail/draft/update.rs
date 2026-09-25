//! CLI command for `omni-dev gmail draft update` (#1924).

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Parser};
use serde::Serialize;

use crate::cli::gmail::draft::create::{load_attachments, read_limited};
use crate::cli::gmail::format::{
    output_as, sanitize_for_terminal, write_scalar_jsonl, JsonlSerialize, OutputFormat,
};
use crate::cli::gmail::helpers::with_modify_scope_hint;
use crate::gmail::client::GmailClient;
use crate::gmail::compose::Mailbox;
use crate::gmail::draft_edit::DraftEdit;
use crate::gmail::drafts_api::DraftsApi;
use crate::gmail::messages_api::MessageFormat;
use crate::gmail::raw_message::decode_raw_message;

/// What `draft update` does with a message, for size-limit refusals.
const UPDATE_ACTION: &str = "update a draft to";

/// Every field-editing flag, which `--raw` conflicts with.
const EDIT_ARGS: [&str; 8] = [
    "to",
    "cc",
    "bcc",
    "subject",
    "body",
    "body_file",
    "attach",
    "remove_attachment",
];

/// Updates a Gmail draft in place, keeping its draft id and its thread.
///
/// Gmail replaces a draft's whole message on every update, so this fetches
/// the stored message, changes only what the flags name, and uploads the
/// result. Every other header, the body, each attachment and the thread
/// membership are kept exactly as they were. `--to`, `--cc` and `--bcc`
/// replace that header's whole list. `--body` replaces the body with plain
/// text; a draft written in Gmail loses its HTML version, with a warning.
///
/// `--raw` replaces the whole message with a `.eml` file instead, such as
/// one written by `draft show --detail raw --out-file`. The draft still
/// keeps its thread.
///
/// Drafts have no version check, so an update overwrites the draft. To avoid
/// overwriting an edit made meanwhile (in Gmail, say), the draft is read
/// again just before the upload and the update is refused if it changed.
/// `--if-message-id` extends that check back to whenever you read it.
///
/// Needs the `gmail.modify` scope (`gmail auth login --modify`). Nothing is
/// ever sent.
#[derive(Parser)]
#[command(group(ArgGroup::new("edit").required(true).multiple(true).args(EDIT_ARGS).arg("raw")))]
pub struct UpdateCommand {
    /// Gmail draft id (the `DRAFT_ID` column of `gmail draft list`).
    pub draft_id: String,

    /// Replace the `To` recipients: `addr@example.com` or
    /// `"Name <addr@example.com>"`. Repeat the flag (or list several after
    /// it) for more recipients.
    #[arg(long, value_name = "ADDR", num_args = 1..)]
    pub to: Vec<String>,

    /// Replace the `Cc` recipients, in the same form as `--to`.
    #[arg(long, value_name = "ADDR", num_args = 1..)]
    pub cc: Vec<String>,

    /// Replace the `Bcc` recipients, in the same form as `--to`.
    #[arg(long, value_name = "ADDR", num_args = 1..)]
    pub bcc: Vec<String>,

    /// Replace the subject.
    #[arg(long)]
    pub subject: Option<String>,

    /// Replace the body with this plain text.
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,

    /// Replace the body with the plain text in this UTF-8 file.
    #[arg(long, value_name = "PATH")]
    pub body_file: Option<PathBuf>,

    /// Attach a file after the existing attachments. Repeat the flag (or
    /// list several after it) for more.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub attach: Vec<PathBuf>,

    /// Remove the attachment with this filename. Repeat the flag (or list
    /// several after it) for more.
    #[arg(long, value_name = "NAME", num_args = 1..)]
    pub remove_attachment: Vec<String>,

    /// Replace the whole message with this RFC 5322 file (`.eml`), uploaded
    /// byte for byte.
    #[arg(long, value_name = "FILE", conflicts_with_all = EDIT_ARGS)]
    pub raw: Option<PathBuf>,

    /// Refuse the update unless the draft's current message id is this one,
    /// e.g. the `MESSAGE_ID` from when you last read the draft.
    #[arg(long, value_name = "ID")]
    pub if_message_id: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl UpdateCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    ///
    /// Parses every flag and reads every local file before any request, so
    /// bad input fails without touching Gmail.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        let change = if let Some(path) = &self.raw {
            Change::Raw(read_limited(path, UPDATE_ACTION)?)
        } else {
            let body = match (self.body, &self.body_file) {
                (Some(body), _) => Some(body),
                (None, Some(path)) => {
                    let bytes = read_limited(path, UPDATE_ACTION)
                        .with_context(|| format!("Failed to read body file {}", path.display()))?;
                    Some(String::from_utf8(bytes).context("The body is not valid UTF-8")?)
                }
                (None, None) => None,
            };
            let body_len = body.as_ref().map_or(0, String::len);
            Change::Edit(DraftEdit {
                to: parse_mailboxes(&self.to)?,
                cc: parse_mailboxes(&self.cc)?,
                bcc: parse_mailboxes(&self.bcc)?,
                subject: self.subject,
                body,
                attach: load_attachments(&self.attach, body_len)?,
                remove_attachments: self.remove_attachment,
            })
        };
        let updated = run_update(
            client,
            &self.draft_id,
            change,
            self.if_message_id.as_deref(),
            &mut std::io::stderr(),
        )
        .await?;
        if output_as(&updated, &self.output)? {
            return Ok(());
        }
        render_updated(&updated, &mut std::io::stdout().lock())
    }
}

/// `None` for a flag that wasn't given, else every value parsed.
fn parse_mailboxes(values: &[String]) -> Result<Option<Vec<Mailbox>>> {
    if values.is_empty() {
        return Ok(None);
    }
    values
        .iter()
        .map(|value| Mailbox::parse(value))
        .collect::<Result<_>>()
        .map(Some)
}

/// The new message, with every local file already read.
#[derive(Debug)]
enum Change {
    /// A complete message, uploaded unchanged.
    Raw(Vec<u8>),
    /// Edits to apply to the stored message.
    Edit(DraftEdit),
}

/// The ids of an updated draft.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct UpdatedDraft {
    /// The draft id, unchanged by the update.
    draft_id: String,
    /// The draft's new message id.
    message_id: String,
    /// The message id the update replaced.
    previous_message_id: String,
    /// The thread the draft belongs to.
    thread_id: String,
}

impl JsonlSerialize for UpdatedDraft {
    fn write_jsonl(&self, out: &mut dyn Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Reads the draft, applies `change`, checks that nobody saved the draft
/// meanwhile, and uploads the result with the draft's thread id.
///
/// Warnings (a dropped HTML version, a thread the draft may leave) go to
/// `warn`. Split from [`UpdateCommand::execute`] so tests can inject a
/// wiremock client and in-memory inputs.
async fn run_update(
    client: &GmailClient,
    draft_id: &str,
    change: Change,
    if_message_id: Option<&str>,
    warn: &mut (dyn Write + Send),
) -> Result<UpdatedDraft> {
    let drafts = DraftsApi::new(client);
    let format = match change {
        Change::Raw(_) => MessageFormat::Minimal,
        Change::Edit(_) => MessageFormat::Raw,
    };
    let current = drafts.get(draft_id, format).await?;
    let read_id = current.message.id.clone();
    if let Some(expected) = if_message_id {
        ensure_unchanged(expected, &read_id)?;
    }
    let thread_id = current
        .message
        .thread_id
        .clone()
        .filter(|id| !id.is_empty());

    let raw = match change {
        // For `--raw` the read above is already the last one before the upload.
        Change::Raw(raw) => raw,
        Change::Edit(edit) => {
            let original = decode_raw_message(&current.message)?;
            let edited = edit.apply(&original)?;
            for warning in &edited.warnings {
                writeln!(warn, "warning: {warning}").context("Failed to write a warning")?;
            }
            // Building the message took a moment; re-read just before the
            // upload to narrow the window for an unseen save.
            let latest = drafts.get(draft_id, MessageFormat::Minimal).await?;
            ensure_unchanged(&read_id, &latest.message.id)?;
            edited.raw
        }
    };

    let updated = drafts
        .update(draft_id, &raw, thread_id.as_deref())
        .await
        .map_err(with_modify_scope_hint)?;
    if let Some(thread_id) = &thread_id {
        if &updated.message.thread_id != thread_id {
            writeln!(
                warn,
                "warning: Gmail moved the draft from thread {thread_id} to thread {}; its subject \
                 or reply headers may no longer match the thread's",
                updated.message.thread_id
            )
            .context("Failed to write a warning")?;
        }
    }
    Ok(UpdatedDraft {
        draft_id: updated.id,
        message_id: updated.message.id,
        previous_message_id: read_id,
        thread_id: updated.message.thread_id,
    })
}

/// Refuses the update when the draft's message id moved from `expected`,
/// which means the draft was saved since it was read.
fn ensure_unchanged(expected: &str, current: &str) -> Result<()> {
    if expected != current {
        bail!(
            "draft changed since it was read (message {expected} is now {current}); nothing was \
             updated. Run `omni-dev gmail draft show` to see the current version, then retry"
        );
    }
    Ok(())
}

/// Prints the ids as aligned `NAME  value` lines.
fn render_updated(updated: &UpdatedDraft, out: &mut dyn Write) -> Result<()> {
    for (name, value) in [
        ("DRAFT_ID           ", &updated.draft_id),
        ("MESSAGE_ID         ", &updated.message_id),
        ("PREVIOUS_MESSAGE_ID", &updated.previous_message_id),
        ("THREAD_ID          ", &updated.thread_id),
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
    use base64::Engine as _;
    use mail_parser::{MessageParser, MimeHeaders};

    const DRAFT_PATH: &str = "/gmail/v1/users/me/drafts/r-1";
    const UPDATE_PATH: &str = "/upload/gmail/v1/users/me/drafts/r-1";

    /// A reply draft with an attachment, as Gmail stores one.
    const REPLY_DRAFT: &str = "MIME-Version: 1.0\r\n\
Date: Mon, 21 Sep 2026 10:00:00 +1000\r\n\
References: <a@example.com>\r\n\
In-Reply-To: <a@example.com>\r\n\
Message-ID: <draft-1@mail.gmail.com>\r\n\
Subject: Re: Report\r\n\
To: alice@example.com\r\n\
Content-Type: multipart/mixed; boundary=\"mixed\"\r\n\
\r\n\
--mixed\r\n\
Content-Type: multipart/alternative; boundary=\"alt\"\r\n\
\r\n\
--alt\r\n\
Content-Type: text/plain; charset=\"UTF-8\"\r\n\
\r\n\
Figures attached.\r\n\
--alt\r\n\
Content-Type: text/html; charset=\"UTF-8\"\r\n\
\r\n\
<div>Figures attached.</div>\r\n\
--alt--\r\n\
--mixed\r\n\
Content-Type: application/pdf; name=\"q3.pdf\"\r\n\
Content-Disposition: attachment; filename=\"q3.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--mixed--\r\n";

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

    /// `drafts.get?format=raw`, returning `raw` as message `message_id` in
    /// thread `t-1`.
    async fn mount_get_raw(server: &wiremock::MockServer, message_id: &str, raw: &str, times: u64) {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(DRAFT_PATH))
            .and(wiremock::matchers::query_param("format", "raw"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "r-1",
                    "message": {"id": message_id, "threadId": "t-1", "raw": encoded},
                })),
            )
            .expect(times)
            .mount(server)
            .await;
    }

    /// `drafts.get?format=minimal`, reporting message `message_id`.
    async fn mount_get_minimal(server: &wiremock::MockServer, message_id: &str, times: u64) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(DRAFT_PATH))
            .and(wiremock::matchers::query_param("format", "minimal"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "r-1",
                    "message": {"id": message_id, "threadId": "t-1"},
                })),
            )
            .expect(times)
            .mount(server)
            .await;
    }

    /// `drafts.update`, answering with message `m-2` in `thread_id`.
    async fn mount_update(server: &wiremock::MockServer, thread_id: &str, times: u64) {
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(UPDATE_PATH))
            .and(wiremock::matchers::query_param("uploadType", "multipart"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "r-1",
                    "message": {"id": "m-2", "threadId": thread_id},
                })),
            )
            .expect(times)
            .mount(server)
            .await;
    }

    /// The uploaded RFC 5322 message and the JSON metadata part.
    async fn uploaded(server: &wiremock::MockServer) -> (String, String) {
        let request = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.method.as_str() == "PUT")
            .unwrap();
        let body = String::from_utf8(request.body).unwrap();
        let metadata = body
            .split("\r\n\r\n")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .unwrap()
            .to_string();
        let message = body
            .split_once("Content-Type: message/rfc822\r\n\r\n")
            .unwrap()
            .1
            .rsplit_once("\r\n--")
            .unwrap()
            .0
            .to_string();
        (metadata, message)
    }

    fn subject_edit(subject: &str) -> Change {
        Change::Edit(DraftEdit {
            subject: Some(subject.to_string()),
            ..DraftEdit::default()
        })
    }

    // ── run_update ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_update_edits_one_field_and_keeps_the_reply_in_its_thread() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 1).await;
        mount_get_minimal(&server, "m-1", 1).await;
        mount_update(&server, "t-1", 1).await;

        let change = Change::Edit(DraftEdit {
            cc: Some(vec![Mailbox::parse("carol@example.com").unwrap()]),
            ..DraftEdit::default()
        });
        let mut warn = Vec::new();
        let updated = run_update(&client, "r-1", change, None, &mut warn)
            .await
            .unwrap();
        assert_eq!(
            updated,
            UpdatedDraft {
                draft_id: "r-1".to_string(),
                message_id: "m-2".to_string(),
                previous_message_id: "m-1".to_string(),
                thread_id: "t-1".to_string(),
            }
        );
        assert!(warn.is_empty(), "{}", String::from_utf8_lossy(&warn));

        let (metadata, message) = uploaded(&server).await;
        assert_eq!(metadata, r#"{"id":"r-1","message":{"threadId":"t-1"}}"#);
        // Only the new Cc header differs from what was stored.
        let without_cc = message.replace("Cc: <carol@example.com>\r\n", "");
        assert_eq!(without_cc, REPLY_DRAFT);
        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(parsed.in_reply_to().as_text(), Some("a@example.com"));
        assert_eq!(
            parsed.attachment(0).unwrap().attachment_name(),
            Some("q3.pdf")
        );
        assert!(parsed.body_html(0).is_some());
    }

    #[tokio::test]
    async fn run_update_refuses_when_the_draft_is_saved_between_read_and_upload() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 1).await;
        // Someone saved the draft in Gmail after it was read.
        mount_get_minimal(&server, "m-9", 1).await;
        mount_update(&server, "t-1", 0).await;

        let err = run_update(
            &client,
            "r-1",
            subject_edit("Re: Report"),
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("draft changed since it was read"),
            "{message}"
        );
        assert!(message.contains("m-1 is now m-9"), "{message}");
    }

    #[tokio::test]
    async fn run_update_honours_if_message_id_before_doing_anything_else() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 1).await;
        mount_get_minimal(&server, "m-1", 0).await;
        mount_update(&server, "t-1", 0).await;

        let err = run_update(
            &client,
            "r-1",
            subject_edit("Re: Report"),
            Some("m-0"),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("draft changed since it was read"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn run_update_proceeds_when_if_message_id_matches() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 1).await;
        mount_get_minimal(&server, "m-1", 1).await;
        mount_update(&server, "t-1", 1).await;

        let updated = run_update(
            &client,
            "r-1",
            subject_edit("RE: report"),
            Some("m-1"),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(updated.message_id, "m-2");
    }

    #[tokio::test]
    async fn run_update_raw_replaces_the_message_and_keeps_the_thread() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 0).await;
        mount_get_minimal(&server, "m-1", 1).await;
        mount_update(&server, "t-1", 1).await;

        let raw = "Subject: Re: Report\nIn-Reply-To: <a@example.com>\n\nUntouched LF body.\n";
        let updated = run_update(
            &client,
            "r-1",
            Change::Raw(raw.as_bytes().to_vec()),
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(updated.thread_id, "t-1");

        let (metadata, message) = uploaded(&server).await;
        assert_eq!(metadata, r#"{"id":"r-1","message":{"threadId":"t-1"}}"#);
        assert_eq!(message, raw);
    }

    #[tokio::test]
    async fn run_update_raw_honours_if_message_id() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_minimal(&server, "m-1", 1).await;
        mount_update(&server, "t-1", 0).await;

        let err = run_update(
            &client,
            "r-1",
            Change::Raw(b"Subject: x\r\n\r\nbody".to_vec()),
            Some("m-0"),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("m-0 is now m-1"), "{err}");
    }

    #[tokio::test]
    async fn run_update_writes_edit_warnings_and_a_thread_move_warning() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 1).await;
        mount_get_minimal(&server, "m-1", 1).await;
        // Gmail filed the result under a new thread.
        mount_update(&server, "t-new", 1).await;

        let change = Change::Edit(DraftEdit {
            subject: Some("Something else".to_string()),
            body: Some("Plain now.".to_string()),
            ..DraftEdit::default()
        });
        let mut warn = Vec::new();
        run_update(&client, "r-1", change, None, &mut warn)
            .await
            .unwrap();
        let warn = String::from_utf8(warn).unwrap();
        assert!(warn.contains("HTML version"), "{warn}");
        assert!(warn.contains("out of its thread"), "{warn}");
        assert!(warn.contains("from thread t-1 to thread t-new"), "{warn}");
    }

    #[tokio::test]
    async fn run_update_rejects_a_bad_edit_before_uploading() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 1).await;
        mount_get_minimal(&server, "m-1", 0).await;
        mount_update(&server, "t-1", 0).await;

        let change = Change::Edit(DraftEdit {
            remove_attachments: vec!["nope.pdf".to_string()],
            ..DraftEdit::default()
        });
        let err = run_update(&client, "r-1", change, None, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("\"q3.pdf\""), "{err}");
    }

    #[tokio::test]
    async fn run_update_turns_a_read_only_403_into_an_actionable_error() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_minimal(&server, "m-1", 1).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(UPDATE_PATH))
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

        let err = run_update(
            &client,
            "r-1",
            Change::Raw(b"Subject: x\r\n\r\nbody".to_vec()),
            None,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("gmail auth login --modify"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn run_update_of_an_unknown_draft_fails_with_the_draft_hint() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(DRAFT_PATH))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        mount_update(&server, "t-1", 0).await;

        let err = run_update(&client, "r-1", subject_edit("x"), None, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("No draft with id"), "{err}");
    }

    // ── UpdateCommand::execute ──────────────────────────────────────

    fn update_command(output: OutputFormat) -> UpdateCommand {
        UpdateCommand {
            draft_id: "r-1".to_string(),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: None,
            body: None,
            body_file: None,
            attach: vec![],
            remove_attachment: vec![],
            raw: None,
            if_message_id: None,
            output,
        }
    }

    #[tokio::test]
    async fn execute_raw_path_uploads_the_file_and_renders_a_table() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_minimal(&server, "m-1", 1).await;
        mount_update(&server, "t-1", 1).await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("draft.eml");
        std::fs::write(&path, b"Subject: Re: Report\r\n\r\nBody.\r\n").unwrap();

        UpdateCommand {
            raw: Some(path),
            ..update_command(OutputFormat::Table)
        }
        .execute(&client)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn execute_edits_from_flags_and_files_and_renders_jsonl() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_get_raw(&server, "m-1", REPLY_DRAFT, 1).await;
        mount_get_minimal(&server, "m-1", 1).await;
        mount_update(&server, "t-1", 1).await;

        let dir = tempfile::tempdir().unwrap();
        let body = dir.path().join("body.txt");
        std::fs::write(&body, "From a file.\n").unwrap();
        let extra = dir.path().join("notes.txt");
        std::fs::write(&extra, "notes").unwrap();

        UpdateCommand {
            to: vec!["bob@example.com".to_string()],
            body_file: Some(body),
            attach: vec![extra],
            remove_attachment: vec!["q3.pdf".to_string()],
            ..update_command(OutputFormat::Jsonl)
        }
        .execute(&client)
        .await
        .unwrap();

        let (_, message) = uploaded(&server).await;
        let parsed = MessageParser::default().parse(message.as_bytes()).unwrap();
        assert_eq!(parsed.body_text(0).as_deref(), Some("From a file.\r\n"));
        let names: Vec<_> = parsed
            .attachments()
            .map(|a| a.attachment_name().unwrap().to_string())
            .collect();
        assert_eq!(names, ["notes.txt"]);
        assert_eq!(
            parsed.to().unwrap().first().unwrap().address(),
            Some("bob@example.com")
        );
    }

    #[tokio::test]
    async fn execute_rejects_a_bad_recipient_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::path_regex("^/(gmail|upload)/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let err = UpdateCommand {
            cc: vec!["broken".to_string()],
            ..update_command(OutputFormat::Table)
        }
        .execute(&client)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("invalid address"), "{err}");
    }

    #[tokio::test]
    async fn execute_reports_an_unreadable_body_file_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::path_regex("^/(gmail|upload)/"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let err = UpdateCommand {
            body_file: Some(missing),
            ..update_command(OutputFormat::Table)
        }
        .execute(&client)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("body file"), "{err}");

        let binary = dir.path().join("binary.txt");
        std::fs::write(&binary, [0xffu8, 0xfe]).unwrap();
        let err = UpdateCommand {
            body_file: Some(binary),
            ..update_command(OutputFormat::Table)
        }
        .execute(&client)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    // ── helpers ──────────────────────────────────────────────────────

    #[test]
    fn parse_mailboxes_distinguishes_absent_from_given() {
        assert!(parse_mailboxes(&[]).unwrap().is_none());
        let parsed = parse_mailboxes(&["Bob <bob@example.com>".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(parsed[0].email, "bob@example.com");
        assert!(parse_mailboxes(&["nope".to_string()]).is_err());
    }

    #[test]
    fn render_updated_labels_all_four_ids() {
        let mut out = Vec::new();
        render_updated(
            &UpdatedDraft {
                draft_id: "r-1".to_string(),
                message_id: "m-2".to_string(),
                previous_message_id: "m-1".to_string(),
                thread_id: "t-1".to_string(),
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "DRAFT_ID             r-1\n\
             MESSAGE_ID           m-2\n\
             PREVIOUS_MESSAGE_ID  m-1\n\
             THREAD_ID            t-1\n"
        );
    }

    // ── clap surface ─────────────────────────────────────────────────

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        update: UpdateCommand,
    }

    fn parse(args: &[&str]) -> Result<UpdateCommand, clap::Error> {
        let argv = std::iter::once("update").chain(args.iter().copied());
        Harness::try_parse_from(argv).map(|h| h.update)
    }

    #[test]
    fn clap_requires_a_draft_id_and_at_least_one_edit() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["r-1"]).is_err());
        assert!(parse(&["r-1", "--if-message-id", "m-1"]).is_err());
        for edit in [
            &["--to", "a@example.com"][..],
            &["--cc", "a@example.com"],
            &["--bcc", "a@example.com"],
            &["--subject", "s"],
            &["--body", "b"],
            &["--body-file", "b.txt"],
            &["--attach", "f"],
            &["--remove-attachment", "f"],
            &["--raw", "m.eml"],
        ] {
            let mut args = vec!["r-1"];
            args.extend(edit);
            assert!(parse(&args).is_ok(), "{edit:?} was refused");
        }
    }

    #[test]
    fn clap_raw_stands_alone_except_for_if_message_id() {
        assert!(parse(&["r-1", "--raw", "m.eml", "--if-message-id", "m-1"]).is_ok());
        for flag in [
            ["--to", "a@example.com"],
            ["--subject", "s"],
            ["--body", "b"],
            ["--attach", "f"],
            ["--remove-attachment", "f"],
        ] {
            let mut args = vec!["r-1", "--raw", "m.eml"];
            args.extend(flag);
            assert!(parse(&args).is_err(), "--raw accepted {flag:?}");
        }
    }

    #[test]
    fn clap_body_and_body_file_conflict() {
        assert!(parse(&["r-1", "--body", "b", "--body-file", "f"]).is_err());
    }

    #[test]
    fn clap_collects_multi_value_flags() {
        let cmd = parse(&[
            "r-1",
            "--remove-attachment",
            "a.pdf",
            "b.pdf",
            "--to",
            "Doe, Jane <j@example.com>",
        ])
        .unwrap();
        assert_eq!(cmd.remove_attachment, ["a.pdf", "b.pdf"]);
        assert_eq!(cmd.to, ["Doe, Jane <j@example.com>"]);
    }
}
