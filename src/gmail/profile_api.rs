//! Gmail Profile API wrapper.
//!
//! `users.getProfile` is a single unpaginated call — no list/cursor shape
//! needed, unlike the other façades. Extracted out of
//! `src/cli/gmail/auth.rs`'s previously-private `ProfileResponse` because
//! `gmail sync` needs the same endpoint for two fields `auth status` never
//! used: `email_address` (mandatory account-identity validation) and
//! `history_id` (the reconciliation watermark snapshot).

use anyhow::Result;
use serde::Deserialize;

use crate::gmail::client::GmailClient;

/// The mailbox's current profile, as returned by `users.getProfile`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Profile {
    /// The authenticated account's email address.
    #[serde(rename = "emailAddress")]
    pub email_address: String,
    /// Total number of messages in the mailbox.
    #[serde(rename = "messagesTotal")]
    pub messages_total: i64,
    /// Total number of threads in the mailbox.
    #[serde(rename = "threadsTotal")]
    pub threads_total: i64,
    /// The mailbox's current `historyId`.
    #[serde(rename = "historyId")]
    pub history_id: String,
}

/// Profile API façade.
#[derive(Debug)]
pub struct ProfileApi<'a> {
    client: &'a GmailClient,
}

impl<'a> ProfileApi<'a> {
    /// Wraps an existing [`GmailClient`] for profile operations.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Fetches the authenticated account's profile.
    pub async fn get(&self) -> Result<Profile> {
        let url = GmailClient::api_url(self.client.base_url(), "/gmail/v1/users/me/profile")?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse users.getProfile response")
            .await
    }
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

    #[tokio::test]
    async fn get_parses_profile_fields() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "emailAddress": "user@example.com",
                    "messagesTotal": 100,
                    "threadsTotal": 42,
                    "historyId": "123456",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let profile = ProfileApi::new(&client).get().await.unwrap();
        assert_eq!(profile.email_address, "user@example.com");
        assert_eq!(profile.messages_total, 100);
        assert_eq!(profile.threads_total, 42);
        assert_eq!(profile.history_id, "123456");
    }

    #[tokio::test]
    async fn get_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/profile"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = ProfileApi::new(&client).get().await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn get_rejects_invalid_base_url() {
        let client = GmailClient::new("not a url", &test_credentials()).unwrap();
        let err = ProfileApi::new(&client).get().await.unwrap_err();
        assert!(err.to_string().contains("Invalid Gmail base URL"));
    }
}
