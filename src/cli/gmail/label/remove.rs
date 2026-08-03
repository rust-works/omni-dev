//! CLI command for `omni-dev gmail label remove`.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::Parser;

use crate::cli::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
use crate::gmail::client::GmailClient;
use crate::gmail::messages_api::MessagesApi;

/// Removes a label from one or more messages.
///
/// Guarded per ADR-0027: interactive confirmation by default, `--force` to
/// skip it, `--dry-run` to preview — removal can lose information the user
/// didn't explicitly restate, unlike `label add`.
#[derive(Parser)]
pub struct RemoveCommand {
    /// Gmail message ids to remove the label from.
    #[arg(required = true)]
    pub message_ids: Vec<String>,

    /// Label id to remove (see `gmail label list` for ids).
    #[arg(long)]
    pub label: String,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Print what would be removed without calling the API.
    #[arg(long)]
    pub dry_run: bool,
}

impl RemoveCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        let mut stdin = io::BufReader::new(io::stdin());
        let mut stdout = io::stdout();
        run_remove(
            client,
            &self.message_ids,
            &self.label,
            self.force,
            self.dry_run,
            &mut stdin,
            &mut stdout,
        )
        .await
    }
}

/// Guards and, if approved, removes `label` from every message in
/// `message_ids`.
///
/// Split from [`RemoveCommand::execute`] so tests can inject a wiremock
/// client and a scripted reader/writer without touching real stdin/stdout.
#[allow(clippy::too_many_arguments)]
async fn run_remove(
    client: &GmailClient,
    message_ids: &[String],
    label: &str,
    force: bool,
    dry_run: bool,
    reader: &mut (dyn BufRead + Send),
    writer: &mut (dyn Write + Send),
) -> Result<()> {
    let prompt = format!(
        "Remove label '{label}' from {} message(s)? [y/N] ",
        message_ids.len()
    );
    let dry_run_message = format!(
        "Would remove label '{label}' from {} message(s).",
        message_ids.len()
    );
    let opts = GuardOptions {
        prompt: &prompt,
        dry_run_message: &dry_run_message,
        force,
        dry_run,
    };

    match guard_destructive_with_io(&opts, reader, writer)? {
        GuardOutcome::Cancelled | GuardOutcome::DryRun => Ok(()),
        GuardOutcome::Proceed => {
            let ids: Vec<&str> = message_ids.iter().map(String::as_str).collect();
            MessagesApi::new(client)
                .batch_modify(&ids, &[], &[label])
                .await?;
            writeln!(
                writer,
                "Removed label '{label}' from {} message(s).",
                ids.len()
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;
    use std::io::Cursor;

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

    #[tokio::test]
    async fn run_remove_dry_run_makes_no_api_call() {
        // No mounted mocks and no token bootstrap — a real request would
        // fail on connection refused, proving no call was attempted.
        let client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        run_remove(
            &client,
            &["m1".to_string()],
            "IMPORTANT",
            false,
            true,
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Would remove label 'IMPORTANT'"));
    }

    #[tokio::test]
    async fn run_remove_cancelled_makes_no_api_call() {
        let client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        run_remove(
            &client,
            &["m1".to_string()],
            "IMPORTANT",
            false,
            false,
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("Cancelled."));
    }

    #[tokio::test]
    async fn run_remove_force_posts_batch_modify_with_remove_label() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "ids": ["m1", "m2"],
                "removeLabelIds": ["UNREAD"],
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        run_remove(
            &client,
            &["m1".to_string(), "m2".to_string()],
            "UNREAD",
            true,
            false,
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Removed label 'UNREAD'"));
    }

    #[tokio::test]
    async fn run_remove_yes_prompt_proceeds() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut input = Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        run_remove(
            &client,
            &["m1".to_string()],
            "UNREAD",
            false,
            false,
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_remove_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = run_remove(
            &client,
            &["m1".to_string()],
            "UNREAD",
            true,
            false,
            &mut input,
            &mut output,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    /// Writer that always fails, used to exercise the "Removed label…"
    /// success-message write-error path.
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated write failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_remove_force_propagates_success_message_write_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let mut input = Cursor::new(Vec::<u8>::new());
        let err = run_remove(
            &client,
            &["m1".to_string()],
            "UNREAD",
            true,
            false,
            &mut input,
            &mut FailingWriter,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    // ── RemoveCommand::execute glue ─────────────────────────────────

    #[tokio::test]
    async fn execute_dry_run_makes_no_api_call_and_never_reads_stdin() {
        // `--dry-run` short-circuits before the guard reads stdin (verified
        // by `confirm.rs`'s own tests), so this is safe to run against the
        // real process stdin/stdout without blocking on input.
        let client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        let cmd = RemoveCommand {
            message_ids: vec!["m1".to_string()],
            label: "IMPORTANT".to_string(),
            force: true,
            dry_run: true,
        };
        cmd.execute(&client).await.unwrap();
    }

    // ── dry-run wins over force ────────────────────────────────────

    #[tokio::test]
    async fn dry_run_wins_over_force_no_api_call() {
        let client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        run_remove(
            &client,
            &["m1".to_string()],
            "IMPORTANT",
            true,
            true,
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("Would remove"));
    }
}
