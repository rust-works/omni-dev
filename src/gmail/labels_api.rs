//! Gmail Labels API wrapper.
//!
//! No pagination — Gmail's `labels.list` returns every label in one call.
//! Label *creation* is not in this issue's endpoint list; the CLI's
//! `label add`/`remove` map to
//! [`MessagesApi::batch_modify`](crate::gmail::messages_api::MessagesApi::batch_modify),
//! not to anything here.

use anyhow::Result;
use url::Url;

use crate::gmail::client::GmailClient;
use crate::gmail::types::{Label, LabelListResponse};

/// Labels API façade.
#[derive(Debug)]
pub struct LabelsApi<'a> {
    client: &'a GmailClient,
}

impl<'a> LabelsApi<'a> {
    /// Wraps an existing [`GmailClient`] for label operations.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Lists every label on the mailbox.
    pub async fn list(&self) -> Result<LabelListResponse> {
        let url = build_labels_list_url(self.client.base_url())?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse labels.list response")
            .await
    }

    /// Fetches a single label by id.
    pub async fn get(&self, id: &str) -> Result<Label> {
        let url = build_label_get_url(self.client.base_url(), id)?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse labels.get response")
            .await
    }
}

fn build_labels_list_url(base_url: &str) -> Result<Url> {
    GmailClient::api_url(base_url, "/gmail/v1/users/me/labels")
}

fn build_label_get_url(base_url: &str, id: &str) -> Result<Url> {
    GmailClient::api_url(base_url, &format!("/gmail/v1/users/me/labels/{id}"))
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

    fn dead_client() -> GmailClient {
        // Routes the session's token endpoint to the same dead address —
        // otherwise `GmailSession` would try to refresh against the real
        // Google token endpoint before the API call is ever attempted.
        let mut client = GmailClient::new("http://127.0.0.1:1", &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            "http://127.0.0.1:1",
        );
        client
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

    // ── URL builders (pure) ──────────────────────────────────────────

    #[test]
    fn build_labels_list_url_is_exact() {
        let url = build_labels_list_url("https://gmail.googleapis.com").unwrap();
        assert_eq!(
            url.as_str(),
            "https://gmail.googleapis.com/gmail/v1/users/me/labels"
        );
    }

    #[test]
    fn build_label_get_url_interpolates_id() {
        let url = build_label_get_url("https://gmail.googleapis.com", "Label_1").unwrap();
        assert_eq!(
            url.as_str(),
            "https://gmail.googleapis.com/gmail/v1/users/me/labels/Label_1"
        );
    }

    #[test]
    fn build_label_get_url_percent_encodes_special_characters_in_user_label_ids() {
        let url = build_label_get_url("https://gmail.googleapis.com", "a b/c").unwrap();
        assert!(!url.as_str().contains(' '));
    }

    #[test]
    fn build_urls_reject_invalid_base_url() {
        assert!(build_labels_list_url("not a url").is_err());
        assert!(build_label_get_url("not a url", "id").is_err());
    }

    // ── list ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_parses_labels_with_and_without_color() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "labels": [
                        {"id": "INBOX", "name": "INBOX", "type": "system"},
                        {
                            "id": "Label_1",
                            "name": "Finance",
                            "type": "user",
                            "color": {"textColor": "#000000", "backgroundColor": "#ffffff"},
                        },
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = LabelsApi::new(&client).list().await.unwrap();
        assert_eq!(result.labels.len(), 2);
        assert!(result.labels[0].is_system());
        assert!(result.labels[1].color.is_some());
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/gmail/v1/users/me/labels"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("nope"))
            .mount(&server)
            .await;

        let err = LabelsApi::new(&client).list().await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn list_propagates_network_errors() {
        // `dead_client()` also points the session's token endpoint at the
        // dead address, so the failure surfaces during token acquisition
        // before the labels.list request is ever attempted.
        let client = dead_client();
        let err = LabelsApi::new(&client).list().await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to obtain a Gmail access token"));
    }

    // ── get ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_builds_correct_url_and_parses_a_single_label() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/gmail/v1/users/me/labels/Label_1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "Label_1",
                    "name": "Finance",
                    "type": "user",
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let label = LabelsApi::new(&client).get("Label_1").await.unwrap();
        assert_eq!(label.name, "Finance");
    }
}
