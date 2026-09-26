//! Shared helpers for Gmail CLI commands.

use anyhow::Result;

use crate::gmail::auth;
use crate::gmail::client::GmailClient;
use crate::gmail::error::GmailError;

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

/// Explains Gmail's scope 403 for a command that writes to the mailbox.
///
/// A `gmail.readonly` account gets `HTTP 403` with reason
/// `insufficientPermissions` from every write endpoint, which says nothing
/// about the fix. This wraps that one error in context naming it: re-consent
/// with `gmail auth login --modify`. The original error stays in the chain.
/// Any other error is returned unchanged.
pub(crate) fn with_modify_scope_hint(err: anyhow::Error) -> anyhow::Error {
    if is_insufficient_scope(&err) {
        err.context(
            "This Gmail account is authorised read-only, and this command needs the \
             `gmail.modify` scope. Re-run `omni-dev gmail auth login --modify` (adding \
             `--account NAME` for a named account) to grant it.",
        )
    } else {
        err
    }
}

/// Whether `err` is Gmail's scope 403: `HTTP 403` with reason
/// `insufficientPermissions`, what a `gmail.readonly` account gets from
/// every write endpoint. Looks through any `.context()` layers.
pub(crate) fn is_insufficient_scope(err: &anyhow::Error) -> bool {
    err.downcast_ref::<GmailError>().is_some_and(|gmail| {
        matches!(gmail, GmailError::ApiRequestFailed { status: 403, .. })
            && gmail.reason() == Some("insufficientPermissions")
    })
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

    fn api_error(status: u16, reason: Option<&str>) -> anyhow::Error {
        GmailError::ApiRequestFailed {
            status,
            body: "Insufficient Permission".to_string(),
            reason: reason.map(str::to_string),
        }
        .into()
    }

    #[test]
    fn with_modify_scope_hint_explains_an_insufficient_scope_403() {
        let err = with_modify_scope_hint(api_error(403, Some("insufficientPermissions")));
        assert!(
            err.to_string().contains("gmail auth login --modify"),
            "{err}"
        );
        // The original error is kept as the cause.
        let cause = err.source().unwrap().to_string();
        assert!(cause.contains("HTTP 403"), "{cause}");
    }

    #[test]
    fn with_modify_scope_hint_leaves_other_errors_alone() {
        for err in [
            api_error(403, Some("rateLimitExceeded")),
            api_error(403, None),
            api_error(400, Some("insufficientPermissions")),
            anyhow::anyhow!("network down"),
        ] {
            let before = err.to_string();
            let after = with_modify_scope_hint(err);
            assert_eq!(after.to_string(), before);
            assert!(after.source().is_none());
        }
    }

    #[test]
    fn is_insufficient_scope_matches_only_the_scope_403() {
        assert!(is_insufficient_scope(&api_error(
            403,
            Some("insufficientPermissions")
        )));
        // Still recognised under added context.
        assert!(is_insufficient_scope(
            &api_error(403, Some("insufficientPermissions")).context("while inserting")
        ));
        for err in [
            api_error(403, Some("rateLimitExceeded")),
            api_error(403, None),
            api_error(400, Some("insufficientPermissions")),
            anyhow::anyhow!("network down"),
        ] {
            assert!(!is_insufficient_scope(&err), "{err}");
        }
    }

    #[test]
    fn create_client_for_unknown_account_errors() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let err = create_client_for(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("unknown Gmail account 'bogus'"));
    }
}
