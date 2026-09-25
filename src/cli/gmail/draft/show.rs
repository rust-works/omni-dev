//! CLI command for `omni-dev gmail draft show`.

use anyhow::Result;
use clap::Parser;

use crate::cli::gmail::read::{
    emit_message, fetch_format, MessageOutputArgs, ReadDetail, ReadOutputFormat, Shown,
};
use crate::gmail::client::GmailClient;
use crate::gmail::drafts_api::DraftsApi;

/// Shows one Gmail draft by its draft id.
///
/// Takes the draft id from `gmail draft list`, not a message id. A draft's
/// message id changes every time the draft is saved, so one saved earlier
/// goes stale; the draft id does not. Output is `gmail read`'s, with the
/// draft id added. Works with a `gmail.readonly` account.
#[derive(Parser)]
pub struct ShowCommand {
    /// Gmail draft id (the `DRAFT_ID` column of `gmail draft list`).
    pub draft_id: String,

    /// Output flags shared with `gmail read`.
    #[command(flatten)]
    pub args: MessageOutputArgs,
}

impl ShowCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        let args = self.args;
        run_show(
            client,
            &self.draft_id,
            args.detail,
            args.out_file.as_deref(),
            &args.output,
            args.fold_quotes,
        )
        .await
    }
}

/// Fetches the draft and emits it through `gmail read`'s output path.
///
/// Split from [`ShowCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_show(
    client: &GmailClient,
    draft_id: &str,
    detail: ReadDetail,
    out_file: Option<&str>,
    output: &ReadOutputFormat,
    fold_quotes: bool,
) -> Result<()> {
    let draft = DraftsApi::new(client)
        .get(draft_id, fetch_format(detail, output))
        .await?;
    emit_message(Shown::Draft(&draft), detail, out_file, output, fold_quotes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;
    use base64::Engine as _;

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
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

    /// Mounts `drafts.get` for draft `r1`, expecting `format` exactly once.
    async fn mount_draft(server: &wiremock::MockServer, format: &str, message: serde_json::Value) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts/r1"))
            .and(wiremock::matchers::query_param("format", format))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "r1", "message": message})),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_show_fetches_each_detail_at_its_own_format_with_a_readonly_client() {
        for (detail, format) in [
            (ReadDetail::Minimal, "minimal"),
            (ReadDetail::Metadata, "metadata"),
            (ReadDetail::Full, "full"),
        ] {
            let server = wiremock::MockServer::start().await;
            let client = client_with_bootstrapped_token(&server).await;
            mount_draft(
                &server,
                format,
                serde_json::json!({"id": "m1", "threadId": "t1"}),
            )
            .await;

            run_show(&client, "r1", detail, None, &ReadOutputFormat::Json, false)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn run_show_raw_out_file_writes_the_stored_bytes_exactly() {
        // CRLF line endings and a non-UTF-8 byte: nothing may be normalised.
        let source: &[u8] = b"To: a@example.com\r\nSubject: Draft\r\n\r\nBody \xff text.\r\n";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_draft(
            &server,
            "raw",
            serde_json::json!({"id": "m1", "threadId": "t1", "raw": encoded}),
        )
        .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("draft.eml");
        run_show(
            &client,
            "r1",
            ReadDetail::Raw,
            Some(path.to_str().unwrap()),
            &ReadOutputFormat::Table,
            false,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), source);
    }

    #[tokio::test]
    async fn run_show_markdown_fetches_raw_whatever_the_detail() {
        let source = "To: a@example.com\r\nBcc: hidden@example.com\r\nSubject: Draft\r\n\
                      Content-Type: text/plain\r\n\r\nDraft body.\r\n";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source);
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_draft(
            &server,
            "raw",
            serde_json::json!({"id": "m1", "raw": encoded}),
        )
        .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("draft.md");
        run_show(
            &client,
            "r1",
            ReadDetail::Metadata,
            Some(path.to_str().unwrap()),
            &ReadOutputFormat::Markdown,
            false,
        )
        .await
        .unwrap();

        let markdown = std::fs::read_to_string(&path).unwrap();
        assert!(
            markdown.starts_with("# Draft\n\n- **Draft-Id:** r1\n"),
            "{markdown}"
        );
        assert!(
            markdown.contains("- **Bcc:** hidden@example.com"),
            "{markdown}"
        );
        assert!(markdown.contains("Draft body."), "{markdown}");
    }

    #[tokio::test]
    async fn run_show_plain_text_out_file_labels_both_ids() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_draft(
            &server,
            "full",
            serde_json::json!({"id": "m1", "threadId": "t1", "snippet": "Hi there"}),
        )
        .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("draft.txt");
        run_show(
            &client,
            "r1",
            ReadDetail::Full,
            Some(path.to_str().unwrap()),
            &ReadOutputFormat::Table,
            false,
        )
        .await
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("Draft-Id: r1\nMessage-Id: m1\nThread-Id: t1\n"),
            "{text}"
        );
        assert!(text.contains("Hi there"), "{text}");
    }

    #[tokio::test]
    async fn run_show_renders_a_table_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        mount_draft(&server, "full", serde_json::json!({"id": "m1"})).await;

        run_show(
            &client,
            "r1",
            ReadDetail::Full,
            None,
            &ReadOutputFormat::Table,
            false,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_show_reports_a_missing_draft_by_id() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/drafts/nope"))
            .respond_with(
                wiremock::ResponseTemplate::new(404)
                    .set_body_string("Requested entity was not found."),
            )
            .mount(&server)
            .await;

        let err = run_show(
            &client,
            "nope",
            ReadDetail::Full,
            None,
            &ReadOutputFormat::Table,
            false,
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("No draft with id \"nope\""), "{message}");
        assert!(message.contains("gmail draft list"), "{message}");
    }
}
