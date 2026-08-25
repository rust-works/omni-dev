//! Shared helpers for Drive CLI commands.

use anyhow::Result;

use crate::drive::auth;
use crate::drive::client::DriveClient;

/// Creates an authenticated Drive API client from environment/settings-resolved credentials.
pub fn create_client() -> Result<DriveClient> {
    create_client_for(None)
}

/// [`create_client`], but honoring the named-account resolution
/// ([ADR-0069](../../../docs/adrs/adr-0069.md)). `account` is `Some(name)`
/// to force that account (the CLI's resolved `--account` value, or an MCP
/// tool's per-call override) or `None` to fall through to ambient
/// `--account`/`OMNI_DEV_DRIVE_ACCOUNT` resolution — [`create_client`]'s
/// exact behavior.
pub fn create_client_for(account: Option<&str>) -> Result<DriveClient> {
    create_client_from(auth::load_credentials_for(account)?)
}

/// Builds a client from already-resolved credentials.
///
/// The dependency-injection seam: commands resolve credentials via
/// [`create_client`] in production, while tests construct a
/// [`DriveCredentials`](auth::DriveCredentials) value (or a wiremock
/// client) directly and never touch the environment.
pub fn create_client_from(credentials: auth::DriveCredentials) -> Result<DriveClient> {
    DriveClient::from_credentials(&credentials)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::utils::secret::Secret;

    #[test]
    fn create_client_from_uses_drive_api_host() {
        let creds = DriveCredentials {
            client_id: "client".to_string(),
            client_secret: Secret::new("secret"),
            refresh_token: Secret::new("refresh"),
            scope: DriveGrantedScopes::READONLY,
        };
        let client = create_client_from(creds).unwrap();
        assert_eq!(client.base_url(), "https://www.googleapis.com");
    }

    #[test]
    fn create_client_for_named_account_uses_that_accounts_credentials() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[
                (
                    "client_id",
                    serde_json::Value::String("work-id".to_string()),
                ),
                (
                    "client_secret",
                    serde_json::Value::String("work-secret".to_string()),
                ),
                (
                    "refresh_token",
                    serde_json::Value::String("work-refresh".to_string()),
                ),
            ],
        )
        .unwrap();

        let client = create_client_for(Some("work")).unwrap();
        assert_eq!(client.base_url(), "https://www.googleapis.com");
    }

    #[test]
    fn create_client_for_unknown_account_errors() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let err = create_client_for(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("unknown Drive account 'bogus'"));
    }
}
