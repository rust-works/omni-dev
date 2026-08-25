//! Named Drive account resolution (mirrors issue #1500/
//! [ADR-0066](../../docs/adrs/adr-0066.md), applied to Drive by
//! [ADR-0069](../../docs/adrs/adr-0069.md)).
//!
//! A second, Drive-specific multi-tenancy axis, orthogonal to `--profile`
//! ([ADR-0045](../../docs/adrs/adr-0045.md)): named accounts in the `drive`
//! block of `settings.json`, selected per invocation via `--account` /
//! [`DRIVE_ACCOUNT_ENV`]. [`resolve_account`] (reads) and
//! [`resolve_account_for_write`] (account-creating writes: `login`,
//! `import`) are the two entry points the credential CRUD layer
//! (`crate::drive::auth`) and the CLI/MCP surfaces route through.
//!
//! Unlike `crate::gmail::account`, there is no pre-existing single-account
//! Drive credential store to preserve — Drive is a brand-new feature — so
//! this module's [`ResolvedAccount`] has an `Unconfigured` variant rather
//! than Gmail's `Legacy`, and there is no
//! `is_first_legacy_to_named_transition`-style shadowing-notice machinery
//! (see [ADR-0069](../../docs/adrs/adr-0069.md) §3).

use anyhow::Result;
use serde::Serialize;

use crate::drive::auth::{DRIVE_CLIENT_ID, DRIVE_CLIENT_SECRET, DRIVE_REFRESH_TOKEN};
use crate::utils::env::EnvSource;
use crate::utils::settings::DriveSettings;

/// Selects the active Drive account for every command, mirroring
/// `OMNI_DEV_PROFILE`. Propagated by the global `--account` flag in
/// `DriveCommand`.
pub const DRIVE_ACCOUNT_ENV: &str = "OMNI_DEV_DRIVE_ACCOUNT";

/// Returns the account named by [`DRIVE_ACCOUNT_ENV`] in `raw`, or `None`
/// when unset or empty.
///
/// Reads the **raw** env only, mirroring
/// [`active_profile_from`](crate::utils::settings::active_profile_from) —
/// pure over the injected source, never resolves through account fallback.
pub fn active_drive_account_from<E: EnvSource>(raw: &E) -> Option<String> {
    raw.var(DRIVE_ACCOUNT_ENV).filter(|s| !s.is_empty())
}

/// Which credential source a Drive command should read from, as decided by
/// [`resolve_account`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAccount {
    /// A named account: `drive.accounts.<name>`.
    Named(String),
    /// No named account applies: either the literal-env bypass is present
    /// (a real credential source with nothing further to resolve), or
    /// `drive.accounts` is empty (nothing configured at all — the
    /// credential-load step must fail loudly with a "run `drive auth
    /// login`" remedy rather than attempt a network call).
    Unconfigured,
}

/// Resolves which Drive account a *read* command (credential load, status,
/// client construction, MCP tools, …) should use.
///
/// `explicit` is the already-resolved `--account`/[`DRIVE_ACCOUNT_ENV`]
/// value, if any (production callers pass the result of calling
/// [`active_drive_account_from`] on `&SystemEnv`, or an MCP per-call
/// override).
///
/// Precedence:
/// 1. A literal `DRIVE_CLIENT_ID`+`DRIVE_CLIENT_SECRET`+`DRIVE_REFRESH_TOKEN`
///    all present in `raw` bypasses account resolution entirely — a
///    scripting/CI convenience, not a migration path (Drive has nothing to
///    migrate from).
/// 2. `explicit`, if set, selects `drive.accounts.<name>`; an unknown name
///    is a hard error listing the configured accounts.
/// 3. No explicit account, `settings.accounts` non-empty: `default_account`
///    if it still names a configured account, else the sole account if
///    exactly one is configured, else a hard error.
/// 4. `settings.accounts` empty: [`ResolvedAccount::Unconfigured`].
///
/// For `drive auth login`/`drive auth import`, which *create* an account
/// rather than select an existing one, see [`resolve_account_for_write`] —
/// this function's rule 2 would otherwise make it impossible to ever
/// configure a brand-new named account, since it would reject the very name
/// being created as "unknown".
pub fn resolve_account<E: EnvSource>(
    raw: &E,
    settings: &DriveSettings,
    explicit: Option<&str>,
) -> Result<ResolvedAccount> {
    if has_literal_credentials(raw) {
        return Ok(ResolvedAccount::Unconfigured);
    }
    if let Some(name) = explicit {
        validate_account(settings, name)?;
        return Ok(ResolvedAccount::Named(name.to_string()));
    }
    if settings.accounts.is_empty() {
        return Ok(ResolvedAccount::Unconfigured);
    }
    resolve_default_account(settings).map(ResolvedAccount::Named)
}

/// Resolves which Drive account a *write* command that can create a new
/// account (`drive auth login`, `drive auth import`) should target.
///
/// Identical to [`resolve_account`] except for rule 2: an explicit
/// `--account`/[`DRIVE_ACCOUNT_ENV`] name need not already be configured —
/// these commands are how an account comes into existence in the first
/// place, so it is always accepted as the target rather than validated.
/// Rules 1, 3, and 4 (literal-env bypass, default/sole-account fallback,
/// empty-accounts fallback) are unchanged, so a plain `drive auth login`
/// with no `--account` still resolves exactly like [`resolve_account`]
/// would.
pub fn resolve_account_for_write<E: EnvSource>(
    raw: &E,
    settings: &DriveSettings,
    explicit: Option<&str>,
) -> Result<ResolvedAccount> {
    if has_literal_credentials(raw) {
        return Ok(ResolvedAccount::Unconfigured);
    }
    if let Some(name) = explicit {
        return Ok(ResolvedAccount::Named(name.to_string()));
    }
    if settings.accounts.is_empty() {
        return Ok(ResolvedAccount::Unconfigured);
    }
    resolve_default_account(settings).map(ResolvedAccount::Named)
}

/// Whether `raw` carries a complete literal credential set directly in the
/// process environment (rule 1 of [`resolve_account`]).
fn has_literal_credentials<E: EnvSource>(raw: &E) -> bool {
    raw.var(DRIVE_CLIENT_ID).is_some()
        && raw.var(DRIVE_CLIENT_SECRET).is_some()
        && raw.var(DRIVE_REFRESH_TOKEN).is_some()
}

/// Validates that `name` is a known Drive account.
///
/// Mirrors
/// [`Settings::validate_profile`](crate::utils::settings::Settings::validate_profile)'s
/// exact message convention (sorted key list, `"(none)"` sentinel). Exposed
/// beyond this module so `drive account set-default` can reuse the same
/// error text rather than duplicating it.
pub fn validate_account(settings: &DriveSettings, name: &str) -> Result<()> {
    if settings.accounts.contains_key(name) {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "unknown Drive account '{name}'; known accounts: {}",
        known_accounts(settings)
    ))
}

/// The configured account names, sorted and comma-joined, or `"(none)"` when
/// none are configured — the shared error-message fragment.
fn known_accounts(settings: &DriveSettings) -> String {
    if settings.accounts.is_empty() {
        "(none)".to_string()
    } else {
        let mut names: Vec<&str> = settings.accounts.keys().map(String::as_str).collect();
        names.sort_unstable();
        names.join(", ")
    }
}

/// Resolves the account to use when no `--account`/[`DRIVE_ACCOUNT_ENV`] is
/// given and at least one account is configured (rule 3 of
/// [`resolve_account`]).
fn resolve_default_account(settings: &DriveSettings) -> Result<String> {
    if let Some(default) = &settings.default_account {
        return if settings.accounts.contains_key(default) {
            Ok(default.clone())
        } else {
            Err(anyhow::anyhow!(
                "configured default Drive account '{default}' no longer exists; known accounts: {}",
                known_accounts(settings)
            ))
        };
    }
    let mut names = settings.accounts.keys();
    if let (Some(sole), None) = (names.next(), names.next()) {
        return Ok(sole.clone());
    }
    Err(anyhow::anyhow!(
        "multiple Drive accounts configured and no default set; pass --account or run \
         `drive account set-default <name>`"
    ))
}

/// One row of `drive account list` / the `drive_account_list` MCP tool.
/// Never carries a secret — only what's safe to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountSummary {
    /// The account name passed to `--account`.
    pub name: String,
    /// The display-only cached email address, if known.
    pub email_address: Option<String>,
    /// The OAuth2 scope this account was authorized with, if known.
    pub scope: Option<String>,
    /// Whether this is `drive.default_account`.
    pub is_default: bool,
}

/// Lists configured Drive accounts, sorted by name.
pub fn list_accounts(settings: &DriveSettings) -> Vec<AccountSummary> {
    let mut names: Vec<&String> = settings.accounts.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let account = &settings.accounts[name];
            AccountSummary {
                name: name.clone(),
                email_address: account.email_address.clone(),
                scope: account.scope.clone(),
                is_default: settings.default_account.as_deref() == Some(name.as_str()),
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;
    use crate::utils::settings::DriveAccountSettings;
    use std::collections::HashMap;

    fn account(email: Option<&str>, scope: Option<&str>) -> DriveAccountSettings {
        DriveAccountSettings {
            client_id: Some("id".to_string()),
            client_secret: Some("secret".to_string()),
            refresh_token: Some("token".to_string()),
            scope: scope.map(str::to_string),
            email_address: email.map(str::to_string),
            chrome_profile_from_email: false,
            browser_command: None,
            write_permissions: crate::utils::settings::WritePermissionsSettings::default(),
        }
    }

    fn settings_with(
        default_account: Option<&str>,
        accounts: &[(&str, DriveAccountSettings)],
    ) -> DriveSettings {
        DriveSettings {
            default_account: default_account.map(str::to_string),
            accounts: accounts
                .iter()
                .map(|(name, acc)| {
                    (
                        (*name).to_string(),
                        DriveAccountSettings {
                            client_id: acc.client_id.clone(),
                            client_secret: acc.client_secret.clone(),
                            refresh_token: acc.refresh_token.clone(),
                            scope: acc.scope.clone(),
                            email_address: acc.email_address.clone(),
                            chrome_profile_from_email: acc.chrome_profile_from_email,
                            browser_command: acc.browser_command.clone(),
                            write_permissions:
                                crate::utils::settings::WritePermissionsSettings::default(),
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn active_drive_account_from_reads_and_trims_empty() {
        assert_eq!(
            active_drive_account_from(&MapEnv::new().with(DRIVE_ACCOUNT_ENV, "work")),
            Some("work".to_string())
        );
        assert_eq!(
            active_drive_account_from(&MapEnv::new().with(DRIVE_ACCOUNT_ENV, "")),
            None
        );
        assert_eq!(active_drive_account_from(&MapEnv::new()), None);
    }

    #[test]
    fn resolve_literal_env_bypasses_account_resolution() {
        let raw = MapEnv::new()
            .with(DRIVE_CLIENT_ID, "id")
            .with(DRIVE_CLIENT_SECRET, "secret")
            .with(DRIVE_REFRESH_TOKEN, "token");
        // Even with an ambiguous multi-account store and no default, or an
        // explicit account, literal env wins outright.
        let settings = settings_with(
            None,
            &[("a", account(None, None)), ("b", account(None, None))],
        );
        assert_eq!(
            resolve_account(&raw, &settings, Some("a")).unwrap(),
            ResolvedAccount::Unconfigured
        );
        assert_eq!(
            resolve_account(&raw, &settings, None).unwrap(),
            ResolvedAccount::Unconfigured
        );
    }

    #[test]
    fn resolve_literal_env_requires_all_three_keys() {
        let raw = MapEnv::new()
            .with(DRIVE_CLIENT_ID, "id")
            .with(DRIVE_CLIENT_SECRET, "secret");
        let settings = settings_with(None, &[("work", account(None, None))]);
        // Missing DRIVE_REFRESH_TOKEN: falls through to normal resolution
        // (rule 3, sole account) rather than the literal-env bypass.
        assert_eq!(
            resolve_account(&raw, &settings, None).unwrap(),
            ResolvedAccount::Named("work".to_string())
        );
    }

    #[test]
    fn resolve_explicit_account_selects_named() {
        let settings = settings_with(None, &[("work", account(None, None))]);
        assert_eq!(
            resolve_account(&MapEnv::new(), &settings, Some("work")).unwrap(),
            ResolvedAccount::Named("work".to_string())
        );
    }

    #[test]
    fn resolve_explicit_unknown_account_errors_with_sorted_list() {
        let settings = settings_with(
            None,
            &[
                ("work", account(None, None)),
                ("personal", account(None, None)),
            ],
        );
        let err = resolve_account(&MapEnv::new(), &settings, Some("bogus")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown Drive account 'bogus'; known accounts: personal, work"
        );
    }

    #[test]
    fn resolve_explicit_unknown_account_reports_none_when_empty() {
        let settings = settings_with(None, &[]);
        let err = resolve_account(&MapEnv::new(), &settings, Some("bogus")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown Drive account 'bogus'; known accounts: (none)"
        );
    }

    // ── resolve_account_for_write (login/import bootstrap a new name) ──

    #[test]
    fn resolve_for_write_explicit_new_name_is_accepted_without_validation() {
        // The whole point: an --account name that doesn't exist yet must
        // succeed for login/import, unlike the read-side resolve_account.
        let settings = settings_with(None, &[]);
        assert_eq!(
            resolve_account_for_write(&MapEnv::new(), &settings, Some("brand-new")).unwrap(),
            ResolvedAccount::Named("brand-new".to_string())
        );
    }

    #[test]
    fn resolve_for_write_explicit_existing_name_still_selects_it() {
        let settings = settings_with(None, &[("work", account(None, None))]);
        assert_eq!(
            resolve_account_for_write(&MapEnv::new(), &settings, Some("work")).unwrap(),
            ResolvedAccount::Named("work".to_string())
        );
    }

    #[test]
    fn resolve_for_write_literal_env_still_bypasses_account_resolution() {
        let raw = MapEnv::new()
            .with(DRIVE_CLIENT_ID, "id")
            .with(DRIVE_CLIENT_SECRET, "secret")
            .with(DRIVE_REFRESH_TOKEN, "token");
        let settings = settings_with(None, &[]);
        assert_eq!(
            resolve_account_for_write(&raw, &settings, Some("brand-new")).unwrap(),
            ResolvedAccount::Unconfigured
        );
    }

    #[test]
    fn resolve_for_write_no_explicit_matches_read_side_fallback_rules() {
        // No --account given: falls back to the same default/sole-account/
        // empty-is-unconfigured rules as resolve_account.
        let sole = settings_with(None, &[("work", account(None, None))]);
        assert_eq!(
            resolve_account_for_write(&MapEnv::new(), &sole, None).unwrap(),
            ResolvedAccount::Named("work".to_string())
        );

        let empty = settings_with(None, &[]);
        assert_eq!(
            resolve_account_for_write(&MapEnv::new(), &empty, None).unwrap(),
            ResolvedAccount::Unconfigured
        );

        let ambiguous = settings_with(
            None,
            &[
                ("work", account(None, None)),
                ("personal", account(None, None)),
            ],
        );
        assert!(resolve_account_for_write(&MapEnv::new(), &ambiguous, None).is_err());
    }

    #[test]
    fn resolve_empty_accounts_is_unconfigured() {
        let settings = settings_with(None, &[]);
        assert_eq!(
            resolve_account(&MapEnv::new(), &settings, None).unwrap(),
            ResolvedAccount::Unconfigured
        );
    }

    #[test]
    fn resolve_no_explicit_uses_valid_default() {
        let settings = settings_with(
            Some("work"),
            &[
                ("work", account(None, None)),
                ("personal", account(None, None)),
            ],
        );
        assert_eq!(
            resolve_account(&MapEnv::new(), &settings, None).unwrap(),
            ResolvedAccount::Named("work".to_string())
        );
    }

    #[test]
    fn resolve_no_explicit_stale_default_errors() {
        let settings = settings_with(Some("gone"), &[("work", account(None, None))]);
        let err = resolve_account(&MapEnv::new(), &settings, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configured default Drive account 'gone' no longer exists; known accounts: work"
        );
    }

    #[test]
    fn resolve_no_explicit_no_default_falls_back_to_sole_account() {
        let settings = settings_with(None, &[("work", account(None, None))]);
        assert_eq!(
            resolve_account(&MapEnv::new(), &settings, None).unwrap(),
            ResolvedAccount::Named("work".to_string())
        );
    }

    #[test]
    fn resolve_no_explicit_no_default_multiple_accounts_errors() {
        let settings = settings_with(
            None,
            &[
                ("work", account(None, None)),
                ("personal", account(None, None)),
            ],
        );
        let err = resolve_account(&MapEnv::new(), &settings, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "multiple Drive accounts configured and no default set; pass --account or run \
             `drive account set-default <name>`"
        );
    }

    #[test]
    fn list_accounts_sorted_and_marks_default() {
        let settings = settings_with(
            Some("work"),
            &[
                (
                    "personal",
                    account(Some("me@gmail.com"), Some("drive.readonly")),
                ),
                ("work", account(Some("me@work.com"), Some("drive.readonly"))),
            ],
        );
        let rows = list_accounts(&settings);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "personal");
        assert_eq!(rows[0].email_address.as_deref(), Some("me@gmail.com"));
        assert_eq!(rows[0].scope.as_deref(), Some("drive.readonly"));
        assert!(!rows[0].is_default);
        assert_eq!(rows[1].name, "work");
        assert!(rows[1].is_default);
    }

    #[test]
    fn list_accounts_empty_when_no_accounts() {
        assert!(list_accounts(&settings_with(None, &[])).is_empty());
    }
}
