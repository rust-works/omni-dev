//! CLI command for `omni-dev gmail label add`.

use anyhow::Result;
use clap::Parser;

use crate::gmail::client::GmailClient;
use crate::gmail::messages_api::MessagesApi;

/// Adds a label to one or more messages.
///
/// Unconditional, no confirmation guard — adding a label is a safe,
/// instantly undoable mutation (matching the Confluence `label add`
/// precedent), unlike `label remove` which can lose information the user
/// didn't explicitly restate.
#[derive(Parser)]
pub struct AddCommand {
    /// Gmail message ids to add the label to.
    #[arg(required = true)]
    pub message_ids: Vec<String>,

    /// Label id to add (see `gmail label list` for ids).
    #[arg(long)]
    pub label: String,
}

impl AddCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `GmailCommand::execute`.
    pub async fn execute(self, client: &GmailClient) -> Result<()> {
        run_add(client, &self.message_ids, &self.label).await
    }
}

/// Adds `label` to every message in `message_ids`.
///
/// Split from [`AddCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_add(client: &GmailClient, message_ids: &[String], label: &str) -> Result<()> {
    let ids: Vec<&str> = message_ids.iter().map(String::as_str).collect();
    MessagesApi::new(client)
        .batch_modify(&ids, &[label], &[])
        .await?;
    println!("Added label '{label}' to {} message(s).", ids.len());
    Ok(())
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
    async fn run_add_posts_batch_modify_with_add_label() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "ids": ["m1", "m2"],
                "addLabelIds": ["IMPORTANT"],
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        run_add(&client, &["m1".to_string(), "m2".to_string()], "IMPORTANT")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_add_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(403).set_body_string("insufficientPermissions"),
            )
            .mount(&server)
            .await;

        let err = run_add(&client, &["m1".to_string()], "IMPORTANT")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── AddCommand::execute glue ──────────────────────────────────

    #[tokio::test]
    async fn execute_passes_ids_and_label_through() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/messages/batchModify",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "ids": ["m1"],
                "addLabelIds": ["STARRED"],
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = AddCommand {
            message_ids: vec!["m1".to_string()],
            label: "STARRED".to_string(),
        };
        cmd.execute(&client).await.unwrap();
    }
}
