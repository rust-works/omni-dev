//! CLI command for `omni-dev gmail thread`.

use anyhow::Result;
use clap::Parser;

use crate::cli::gmail::format::{output_as, OutputFormat};
use crate::cli::gmail::search::render_search_table;
use crate::gmail::client::GmailClient;
use crate::gmail::messages_api::MessageSummary;
use crate::gmail::threads_api::{ThreadFormat, ThreadsApi};
use crate::gmail::types::Thread;

/// Reads a Gmail thread (mirrors the `gmail_thread_read` MCP tool).
#[derive(Parser)]
pub struct ThreadCommand {
    /// Gmail thread id.
    pub thread_id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ThreadCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_thread(client, &self.thread_id, &self.output).await
    }
}

/// Fetches the thread and emits it in the requested format.
///
/// Split from [`ThreadCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_thread(client: &GmailClient, thread_id: &str, output: &OutputFormat) -> Result<()> {
    let thread = ThreadsApi::new(client)
        .get(thread_id, ThreadFormat::Full)
        .await?;
    if output_as(&thread, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let rows = thread_rows(&thread);
    render_search_table(&rows, &mut handle)
}

/// Reuses the search table renderer for the thread's per-message rows —
/// each message is already fully fetched (no second API call needed).
fn thread_rows(thread: &Thread) -> Vec<MessageSummary> {
    thread
        .messages
        .iter()
        .map(MessageSummary::from_message)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

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

    #[test]
    fn thread_rows_builds_one_summary_per_message() {
        let thread = Thread {
            id: "t1".to_string(),
            messages: vec![
                crate::gmail::types::Message {
                    id: "m1".to_string(),
                    ..Default::default()
                },
                crate::gmail::types::Message {
                    id: "m2".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let rows = thread_rows(&thread);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "m1");
        assert_eq!(rows[1].id, "m2");
    }

    // ── run_thread ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_thread_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads/t1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "t1",
                    "messages": [{"id": "m1", "threadId": "t1"}],
                })),
            )
            .mount(&server)
            .await;

        run_thread(&client, "t1", &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_thread_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads/t1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "t1",
                })),
            )
            .mount(&server)
            .await;

        run_thread(&client, "t1", &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_thread_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads/t1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = run_thread(&client, "t1", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── ThreadCommand::execute glue ────────────────────────────────

    #[tokio::test]
    async fn execute_passes_thread_id_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/threads/t42"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "t42"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cmd = ThreadCommand {
            thread_id: "t42".to_string(),
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
