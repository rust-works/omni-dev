//! Gmail send-as settings wrapper.
//!
//! `users.settings.sendAs.list` returns the account's primary address and
//! every send-as alias in one unpaginated call (1 quota unit, allowed by
//! `gmail.readonly`). `gmail draft create --reply-all` uses it to leave the
//! account's own addresses out of a reply (#1954). Read-only: nothing here
//! creates, edits or verifies an alias.

use anyhow::Result;
use serde::Deserialize;

use crate::gmail::client::GmailClient;

/// One address the account can send as.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SendAs {
    /// The address that appears in `From`.
    #[serde(rename = "sendAsEmail")]
    pub send_as_email: String,
}

/// The `users.settings.sendAs.list` response body.
#[derive(Debug, Deserialize)]
struct SendAsList {
    #[serde(rename = "sendAs", default)]
    send_as: Vec<SendAs>,
}

/// Send-as settings API façade.
#[derive(Debug)]
pub struct SendAsApi<'a> {
    client: &'a GmailClient,
}

impl<'a> SendAsApi<'a> {
    /// Wraps an existing [`GmailClient`] for send-as settings.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Lists the primary address and every send-as alias.
    pub async fn list(&self) -> Result<Vec<SendAs>> {
        let url =
            GmailClient::api_url(self.client.base_url(), "/gmail/v1/users/me/settings/sendAs")?;
        let list: SendAsList = self
            .client
            .get_parsed(
                url.as_str(),
                "Failed to parse users.settings.sendAs.list response",
            )
            .await?;
        Ok(list.send_as)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

    const SEND_AS_PATH: &str = "/gmail/v1/users/me/settings/sendAs";

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
    async fn list_parses_the_primary_address_and_aliases() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "sendAs": [
                        {
                            "sendAsEmail": "me@example.com",
                            "displayName": "Me",
                            "isPrimary": true,
                            "isDefault": true,
                        },
                        {
                            "sendAsEmail": "alias@example.org",
                            "verificationStatus": "accepted",
                        },
                    ],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let list = SendAsApi::new(&client).list().await.unwrap();
        assert_eq!(
            list,
            [
                SendAs {
                    send_as_email: "me@example.com".to_string(),
                },
                SendAs {
                    send_as_email: "alias@example.org".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn list_treats_a_missing_array_as_empty() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        assert!(SendAsApi::new(&client).list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = SendAsApi::new(&client).list().await.unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
    }
}
