//! Shared helpers for Gmail CLI commands.

use anyhow::Result;

use crate::gmail::auth;
use crate::gmail::client::GmailClient;

/// Creates an authenticated Gmail API client from environment/settings-resolved credentials.
pub fn create_client() -> Result<GmailClient> {
    create_client_from(auth::load_credentials()?)
}

/// Builds a client from already-resolved credentials.
///
/// The dependency-injection seam: commands resolve credentials via
/// [`create_client`] in production, while tests construct a
/// [`GmailCredentials`](auth::GmailCredentials) value (or a wiremock
/// client) directly and never touch the environment.
pub fn create_client_from(credentials: auth::GmailCredentials) -> Result<GmailClient> {
    GmailClient::from_credentials(&credentials)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

    #[test]
    fn create_client_from_uses_gmail_api_host() {
        let creds = GmailCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: GmailScope::ReadOnly,
        };
        let client = create_client_from(creds).unwrap();
        assert_eq!(client.base_url(), "https://gmail.googleapis.com");
    }
}
