//! Shared helpers for Drive CLI commands.

use anyhow::Result;

use crate::drive::account::ResolvedAccount;
use crate::drive::auth;
use crate::drive::client::DriveClient;
use crate::drive::write_gate::FolderPermissionRule;
use crate::utils::settings::Settings;

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

/// Reads the active account's `write_permissions.rules` from
/// `~/.omni-dev/settings.json` (issue #1574). An
/// [`ResolvedAccount::Unconfigured`] account has no `write_permissions`
/// block to read, so it resolves to an empty rule set — every write is
/// refused, per the gate's default policy.
///
/// Shared by `drive create`/`upload`/`edit`/`permissions show`/
/// `permissions check` — previously each of the five reimplemented this
/// identically.
pub fn active_account_rules() -> Result<Vec<FolderPermissionRule>> {
    let settings = Settings::load().unwrap_or_default();
    let resolved = auth::resolve(&settings.drive, None)?;
    Ok(match &resolved {
        ResolvedAccount::Named(name) => settings
            .drive
            .accounts
            .get(name)
            .map(|a| a.write_permissions.rules.clone())
            .unwrap_or_default(),
        ResolvedAccount::Unconfigured => Vec::new(),
    })
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

    // ── active_account_rules ────────────────────────────────────────────

    #[test]
    fn active_account_rules_unconfigured_account_is_empty() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        assert!(active_account_rules().unwrap().is_empty());
    }

    #[test]
    fn active_account_rules_reads_the_sole_configured_accounts_rules() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[(
                "write_permissions",
                serde_json::json!({
                    "rules": [{
                        "folder_id": "folder-1",
                        "recursive": true,
                        "allow": ["create"],
                    }],
                }),
            )],
        )
        .unwrap();

        let rules = active_account_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].folder_id.as_deref(), Some("folder-1"));
        assert!(rules[0]
            .allow
            .contains(&crate::drive::write_gate::DriveOperation::Create));
    }
}
