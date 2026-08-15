//! Drive About API wrapper.
//!
//! `about.get`, used solely by `drive auth status`'s live authentication
//! check (mirrors `crate::gmail::profile_api::ProfileApi`, whose
//! `users.getProfile` plays the same role for `gmail auth status`).

use anyhow::Result;
use serde::Deserialize;
use url::Url;

use crate::drive::client::DriveClient;

/// The authenticated user's identity, as embedded in `about.get`'s `user`
/// field.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AboutUser {
    /// The user's email address.
    #[serde(default, rename = "emailAddress")]
    pub email_address: Option<String>,
    /// The user's display name.
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
}

/// Response for `GET /drive/v3/about`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct About {
    /// The authenticated user's identity.
    pub user: AboutUser,
}

/// About API façade.
#[derive(Debug)]
pub struct AboutApi<'a> {
    client: &'a DriveClient,
}

impl<'a> AboutApi<'a> {
    /// Wraps an existing [`DriveClient`] for `about` operations.
    #[must_use]
    pub fn new(client: &'a DriveClient) -> Self {
        Self { client }
    }

    /// Fetches the authenticated user's identity.
    pub async fn get(&self) -> Result<About> {
        let url = build_about_url(self.client.base_url())?;
        self.client
            .get_parsed(url.as_str(), "Failed to parse about.get response")
            .await
    }
}

fn build_about_url(base_url: &str) -> Result<Url> {
    let mut url = DriveClient::api_url(base_url, "/drive/v3/about")?;
    url.query_pairs_mut()
        .append_pair("fields", "user(emailAddress,displayName)");
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, SCOPE_READONLY};
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: SCOPE_READONLY.to_string(),
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
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

        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    #[test]
    fn build_about_url_includes_fields_query_param() {
        let url = build_about_url("https://www.googleapis.com").unwrap();
        assert!(url.as_str().contains("/drive/v3/about"));
        assert!(url.as_str().contains("fields="));
    }

    #[tokio::test]
    async fn get_parses_user_fields() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": {
                        "emailAddress": "user@example.com",
                        "displayName": "User Name",
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let about = AboutApi::new(&client).get().await.unwrap();
        assert_eq!(
            about.user.email_address.as_deref(),
            Some("user@example.com")
        );
        assert_eq!(about.user.display_name.as_deref(), Some("User Name"));
    }

    #[tokio::test]
    async fn get_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/about"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = AboutApi::new(&client).get().await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn get_rejects_invalid_base_url() {
        let client = DriveClient::new("not a url", &test_credentials()).unwrap();
        let err = AboutApi::new(&client).get().await.unwrap_err();
        assert!(err.to_string().contains("Invalid Drive base URL"));
    }
}
