//! Shared helpers for Gmail CLI commands.

use anyhow::Result;

use crate::gmail::auth;
use crate::gmail::client::GmailClient;

/// Creates an authenticated Gmail API client from environment/settings-resolved credentials.
pub fn create_client() -> Result<GmailClient> {
    create_client_for(None)
}

/// [`create_client`], but honoring the named-account resolution added by
/// issue #1500. `account` is `Some(name)` to force that account (the CLI's
/// resolved `--account` value, or an MCP tool's per-call override) or
/// `None` to fall through to ambient `--account`/`OMNI_DEV_GMAIL_ACCOUNT`
/// resolution — [`create_client`]'s exact behavior.
pub fn create_client_for(account: Option<&str>) -> Result<GmailClient> {
    create_client_from(auth::load_credentials_for(account)?)
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

/// The one-time notice printed at the empty→non-empty `gmail.accounts`
/// transition (issue #1500, [ADR-0066](../../../docs/adrs/adr-0066.md)):
/// from this point on, a no-`--account` invocation resolves through the
/// named-account rules and no longer falls back to legacy credentials.
/// Shared by `gmail auth login --account` and `gmail account
/// import-legacy`, the two commands that can trigger the transition.
pub fn print_shadowing_notice() {
    eprintln!(
        "note: legacy Gmail credentials are now shadowed for invocations without --account. \
         Run `gmail account import-legacy` to migrate any other legacy account, or \
         `gmail auth logout` to remove the old credentials once every mailbox you use is \
         migrated."
    );
}

/// Whether `label_ids` contains any of `targets` — a plain membership
/// check shared by `gmail insert`'s system-label replay filter
/// (`insert/labels.rs`'s `lands_in_inbox_or_unread`/`lands_in_trash_or_spam`)
/// and `gmail sync`'s `--exclude-label` matching (`sync/engine.rs`), so a
/// future change to label-comparison semantics (case normalization, etc.)
/// only needs fixing in one place. Generic over `T: AsRef<str>` so callers
/// can pass either a `&[&str]` literal (a small fixed set of system
/// labels) or a `&[String]` (a caller-supplied list like
/// `--exclude-label`) without an intermediate allocation.
pub(crate) fn label_ids_contain_any<T: AsRef<str>>(label_ids: &[String], targets: &[T]) -> bool {
    targets
        .iter()
        .any(|target| label_ids.iter().any(|label| label == target.as_ref()))
}

#[cfg(test)]
mod label_membership_tests {
    use super::label_ids_contain_any;

    #[test]
    fn label_ids_contain_any_matches_str_targets() {
        let label_ids = vec!["INBOX".to_string(), "UNREAD".to_string()];
        assert!(label_ids_contain_any(&label_ids, &["UNREAD", "STARRED"]));
        assert!(!label_ids_contain_any(&label_ids, &["SPAM", "TRASH"]));
    }

    #[test]
    fn label_ids_contain_any_matches_string_targets() {
        let label_ids = vec!["SPAM".to_string()];
        let targets = vec!["SPAM".to_string(), "TRASH".to_string()];
        assert!(label_ids_contain_any(&label_ids, &targets));
    }

    #[test]
    fn label_ids_contain_any_is_false_for_empty_targets() {
        let label_ids = vec!["INBOX".to_string()];
        let targets: Vec<String> = Vec::new();
        assert!(!label_ids_contain_any(&label_ids, &targets));
    }
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

    #[test]
    fn create_client_for_named_account_uses_that_accounts_credentials() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_gmail_account(
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
        assert_eq!(client.base_url(), "https://gmail.googleapis.com");
    }

    #[test]
    fn create_client_for_unknown_account_errors() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let err = create_client_for(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("unknown Gmail account 'bogus'"));
    }
}
