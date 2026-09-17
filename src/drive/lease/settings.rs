//! Global policy resolution for `drive lease acquire`/`restore`
//! ([ADR-0080](../../../docs/adrs/adr-0080.md) §13, issue #1677).
//!
//! Mirrors `crate::claude::backend`'s `resolve_model`/
//! `resolve_structured_output_disabled` shape: every resolver here reads the
//! environment only through an [`EnvSource`], so production callers pass
//! `&SettingsEnv::load()` (letting a value also come from a `settings.json`
//! `env` bundle or profile) while tests inject a pure `MapEnv`.
//!
//! Precedence, stopping at the first value found, for all four settings:
//!
//! 1. The CLI flag, when given explicitly.
//! 2. The dedicated env var below.
//! 3. The matching field of [`LeaseSettings`] (the `lease` section of
//!    `settings.json`).
//! 4. A hard-coded default.
//!
//! [`resolve_auth_policy`] and [`resolve_allow_headless`] are OR-chains
//! rather than first-non-empty-wins: any layer saying "yes" wins, with no way
//! for a lower layer's "yes" to be forced back to "no" — the same
//! additive-only shape as this codebase's other opt-in/opt-out escape
//! hatches (e.g. `--claude-cli-allow-tools`).

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::drive::lease::acquire::{MAX_EXPIRY_MINUTES, MIN_EXPIRY_MINUTES};
use crate::drive::lease::authenticate::AuthPolicy;
use crate::utils::env::EnvSource;
use crate::utils::settings::LeaseSettings;

/// Default lease expiry when no layer sets one (ADR-0080 §5).
pub(crate) const DEFAULT_EXPIRY_MINUTES: i64 = 30;

/// Env var overriding the default `--expiry-minutes`.
pub(crate) const LEASE_EXPIRY_MINUTES_ENV: &str = "OMNI_DEV_DRIVE_LEASE_EXPIRY_MINUTES";
/// Env var overriding the default `--backup-dir`.
pub(crate) const LEASE_BACKUP_DIR_ENV: &str = "OMNI_DEV_DRIVE_LEASE_BACKUP_DIR";
/// Env var overriding the default authentication policy. Truthy values are
/// `1`, `true`, and `yes` (trimmed, case-insensitive), matching
/// `resolve_structured_output_disabled`'s convention.
pub(crate) const LEASE_BIOMETRICS_ONLY_ENV: &str = "OMNI_DEV_DRIVE_LEASE_BIOMETRICS_ONLY";
/// Env var enabling the headless/off-macOS opt-out (ADR-0080 §8). Same
/// truthy-value convention as [`LEASE_BIOMETRICS_ONLY_ENV`].
pub(crate) const LEASE_ALLOW_HEADLESS_ENV: &str = "OMNI_DEV_DRIVE_LEASE_ALLOW_HEADLESS";

/// Returns `var`'s value when it is set and non-empty — see
/// `crate::claude::backend::non_empty_var`, which this mirrors.
fn non_empty_var(env: &impl EnvSource, key: &str) -> Option<String> {
    env.var(key).filter(|v| !v.is_empty())
}

/// Parses `var` as a truthy boolean: `1`, `true`, or `yes` (trimmed,
/// case-insensitive) is `true`; `0`, `false`, or `no` is `false`; unset or
/// empty is `false` with no warning (the ordinary "not set" case). Anything
/// else — a typo like `treu` — is also treated as `false`, but logs a
/// warning first: a silently-discarded, unrecognized value is exactly the
/// broken-configuration case docs/STYLE_GUIDE.md's silent-discard rule
/// warns against (issue #1677 review finding). Otherwise mirrors
/// `crate::claude::backend::resolve_structured_output_disabled`.
fn truthy_var(env: &impl EnvSource, key: &str) -> bool {
    let Some(raw) = non_empty_var(env, key) else {
        return false;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => true,
        "0" | "false" | "no" => false,
        _ => {
            tracing::warn!(
                "{key}={raw:?} is not a recognized boolean (expected 1/true/yes or \
                 0/false/no); treating it as unset"
            );
            false
        }
    }
}

/// `<state dir>/omni-dev/drive-backups` — a sibling of the request log and
/// lease ledger, same posture. The hard-coded default at the bottom of
/// [`resolve_backup_dir`]'s chain.
fn default_backup_dir() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("could not resolve the state/data directory for the default backup directory")?;
    Ok(base.join("omni-dev").join("drive-backups"))
}

/// Resolves `--expiry-minutes`, range-checked against the same
/// [`MIN_EXPIRY_MINUTES`]/[`MAX_EXPIRY_MINUTES`] bounds the CLI's own
/// `value_parser` enforces on an explicit flag. Checking here too — not just
/// in `crate::drive::lease::acquire::acquire_inner`'s own defense-in-depth
/// pass — matters because the caller turns this into a `chrono::Duration`
/// immediately (`chrono::Duration::minutes` panics on an out-of-range `i64`
/// long before `acquire_inner` ever runs), so an unchecked env/settings.json
/// value would crash the process instead of erroring cleanly (issue #1677
/// review finding). An unparseable env value is treated as unset, the same
/// spirit as an empty string.
pub(crate) fn resolve_expiry_minutes(
    explicit: Option<i64>,
    env: &impl EnvSource,
    settings: &LeaseSettings,
) -> Result<i64> {
    let fallback = || {
        settings
            .default_expiry_minutes
            .unwrap_or(DEFAULT_EXPIRY_MINUTES)
    };
    let value = if let Some(value) = explicit {
        value
    } else if let Some(raw) = non_empty_var(env, LEASE_EXPIRY_MINUTES_ENV) {
        if let Ok(value) = raw.parse() {
            value
        } else {
            tracing::warn!(
                "{LEASE_EXPIRY_MINUTES_ENV}={raw:?} is not a valid integer; falling back to \
                 settings.json/the hard-coded default"
            );
            fallback()
        }
    } else {
        fallback()
    };
    if !(MIN_EXPIRY_MINUTES..=MAX_EXPIRY_MINUTES).contains(&value) {
        anyhow::bail!(
            "lease expiry must be between {MIN_EXPIRY_MINUTES} and {MAX_EXPIRY_MINUTES} \
             minutes (24 hours), got {value} (from {LEASE_EXPIRY_MINUTES_ENV} or \
             settings.json's lease.default_expiry_minutes)"
        );
    }
    Ok(value)
}

/// Resolves `dir` to an absolute path against the current working
/// directory, without touching the filesystem or requiring `dir` to exist —
/// a relative `--backup-dir`/env/settings.json value must not be stored
/// relative in the ledger, since a later `prune`/`restore` can run from a
/// different cwd (issue #1692).
fn absolutize(dir: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&dir).with_context(|| {
        format!(
            "Failed to resolve backup directory {} to an absolute path",
            dir.display()
        )
    })
}

/// Resolves `--backup-dir`.
pub(crate) fn resolve_backup_dir(
    explicit: Option<PathBuf>,
    env: &impl EnvSource,
    settings: &LeaseSettings,
) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return absolutize(dir);
    }
    if let Some(dir) = non_empty_var(env, LEASE_BACKUP_DIR_ENV) {
        return absolutize(PathBuf::from(dir));
    }
    if let Some(dir) = settings.backup_dir.clone() {
        return absolutize(dir);
    }
    default_backup_dir()
}

/// Resolves the authentication policy from `--biometrics-only`. Any layer
/// saying "biometrics-only" wins; there is no way for a lower layer's `true`
/// to be overridden back to device-owner.
pub(crate) fn resolve_auth_policy(
    cli_biometrics_only: bool,
    env: &impl EnvSource,
    settings: &LeaseSettings,
) -> AuthPolicy {
    if cli_biometrics_only || truthy_var(env, LEASE_BIOMETRICS_ONLY_ENV) || settings.biometrics_only
    {
        AuthPolicy::BiometricsOnly
    } else {
        AuthPolicy::DeviceOwner
    }
}

/// Resolves the headless/off-macOS opt-out (ADR-0080 §8). Any layer saying
/// `true` wins. An explicit `--allow-headless` is a deliberate, visible,
/// per-invocation act and warrants no extra warning; but because this waives
/// the human-presence authentication guarantee rather than merely widening a
/// sandbox, a `true` from the *ambient* layers — an inherited env var or a
/// machine-wide `settings.json` — is warned about, naming which layer
/// triggered it, the same way `--claude-cli-allow-tools`' escape hatch names
/// its own source (issue #1677 review finding).
pub(crate) fn resolve_allow_headless(
    cli_flag: bool,
    env: &impl EnvSource,
    settings: &LeaseSettings,
) -> bool {
    if cli_flag {
        return true;
    }
    if truthy_var(env, LEASE_ALLOW_HEADLESS_ENV) {
        tracing::warn!(
            "drive lease: the human-presence authentication guarantee is waived for this \
             invocation because {LEASE_ALLOW_HEADLESS_ENV} is set (ADR-0080 §8)"
        );
        return true;
    }
    if settings.allow_headless {
        tracing::warn!(
            "drive lease: the human-presence authentication guarantee is waived because \
             settings.json's lease.allow_headless is true (ADR-0080 §8/§13) — this applies to \
             every invocation on this machine until that setting is turned off"
        );
        return true;
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    fn settings() -> LeaseSettings {
        LeaseSettings::default()
    }

    // ── resolve_expiry_minutes ──

    #[test]
    fn expiry_minutes_explicit_wins() {
        let env = MapEnv::new().with(LEASE_EXPIRY_MINUTES_ENV, "10");
        let mut s = settings();
        s.default_expiry_minutes = Some(20);
        assert_eq!(resolve_expiry_minutes(Some(5), &env, &s).unwrap(), 5);
    }

    #[test]
    fn expiry_minutes_env_beats_settings() {
        let env = MapEnv::new().with(LEASE_EXPIRY_MINUTES_ENV, "10");
        let mut s = settings();
        s.default_expiry_minutes = Some(20);
        assert_eq!(resolve_expiry_minutes(None, &env, &s).unwrap(), 10);
    }

    #[test]
    fn expiry_minutes_settings_beats_hardcoded_default() {
        let mut s = settings();
        s.default_expiry_minutes = Some(20);
        assert_eq!(
            resolve_expiry_minutes(None, &MapEnv::new(), &s).unwrap(),
            20
        );
    }

    #[test]
    fn expiry_minutes_falls_back_to_hardcoded_default() {
        assert_eq!(
            resolve_expiry_minutes(None, &MapEnv::new(), &settings()).unwrap(),
            DEFAULT_EXPIRY_MINUTES
        );
    }

    #[test]
    fn expiry_minutes_unparseable_env_is_treated_as_unset() {
        let env = MapEnv::new().with(LEASE_EXPIRY_MINUTES_ENV, "not-a-number");
        let mut s = settings();
        s.default_expiry_minutes = Some(20);
        assert_eq!(resolve_expiry_minutes(None, &env, &s).unwrap(), 20);
    }

    #[test]
    fn expiry_minutes_empty_env_is_treated_as_unset() {
        let env = MapEnv::new().with(LEASE_EXPIRY_MINUTES_ENV, "");
        let mut s = settings();
        s.default_expiry_minutes = Some(20);
        assert_eq!(resolve_expiry_minutes(None, &env, &s).unwrap(), 20);
    }

    #[test]
    fn expiry_minutes_out_of_range_env_value_is_a_clean_error_not_a_panic() {
        let env = MapEnv::new().with(LEASE_EXPIRY_MINUTES_ENV, "9223372036854775807");
        let err = resolve_expiry_minutes(None, &env, &settings()).unwrap_err();
        assert!(err.to_string().contains("must be between"), "{err}");
    }

    #[test]
    fn expiry_minutes_out_of_range_settings_value_is_a_clean_error_not_a_panic() {
        let mut s = settings();
        s.default_expiry_minutes = Some(0);
        let err = resolve_expiry_minutes(None, &MapEnv::new(), &s).unwrap_err();
        assert!(err.to_string().contains("must be between"), "{err}");
    }

    // ── resolve_backup_dir ──

    #[test]
    fn backup_dir_explicit_wins() {
        let env = MapEnv::new().with(LEASE_BACKUP_DIR_ENV, "/from/env");
        let mut s = settings();
        s.backup_dir = Some(PathBuf::from("/from/settings"));
        assert_eq!(
            resolve_backup_dir(Some(PathBuf::from("/from/cli")), &env, &s).unwrap(),
            PathBuf::from("/from/cli")
        );
    }

    #[test]
    fn backup_dir_env_beats_settings() {
        let env = MapEnv::new().with(LEASE_BACKUP_DIR_ENV, "/from/env");
        let mut s = settings();
        s.backup_dir = Some(PathBuf::from("/from/settings"));
        assert_eq!(
            resolve_backup_dir(None, &env, &s).unwrap(),
            PathBuf::from("/from/env")
        );
    }

    #[test]
    fn backup_dir_settings_beats_hardcoded_default() {
        let mut s = settings();
        s.backup_dir = Some(PathBuf::from("/from/settings"));
        assert_eq!(
            resolve_backup_dir(None, &MapEnv::new(), &s).unwrap(),
            PathBuf::from("/from/settings")
        );
    }

    #[test]
    fn backup_dir_falls_back_to_hardcoded_default() {
        let resolved = resolve_backup_dir(None, &MapEnv::new(), &settings()).unwrap();
        assert!(resolved.ends_with("omni-dev/drive-backups"));
    }

    #[test]
    fn backup_dir_explicit_relative_is_absolutized() {
        let resolved = resolve_backup_dir(
            Some(PathBuf::from("relative/cli")),
            &MapEnv::new(),
            &settings(),
        )
        .unwrap();
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert!(resolved.ends_with("relative/cli"), "{}", resolved.display());
    }

    #[test]
    fn backup_dir_env_relative_is_absolutized() {
        let env = MapEnv::new().with(LEASE_BACKUP_DIR_ENV, "relative/env");
        let resolved = resolve_backup_dir(None, &env, &settings()).unwrap();
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert!(resolved.ends_with("relative/env"), "{}", resolved.display());
    }

    #[test]
    fn backup_dir_settings_relative_is_absolutized() {
        let mut s = settings();
        s.backup_dir = Some(PathBuf::from("relative/settings"));
        let resolved = resolve_backup_dir(None, &MapEnv::new(), &s).unwrap();
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert!(
            resolved.ends_with("relative/settings"),
            "{}",
            resolved.display()
        );
    }

    // ── resolve_auth_policy ──

    #[test]
    fn auth_policy_defaults_to_device_owner() {
        assert_eq!(
            resolve_auth_policy(false, &MapEnv::new(), &settings()),
            AuthPolicy::DeviceOwner
        );
    }

    #[test]
    fn auth_policy_cli_flag_selects_biometrics_only() {
        assert_eq!(
            resolve_auth_policy(true, &MapEnv::new(), &settings()),
            AuthPolicy::BiometricsOnly
        );
    }

    #[test]
    fn auth_policy_env_selects_biometrics_only() {
        let env = MapEnv::new().with(LEASE_BIOMETRICS_ONLY_ENV, "true");
        assert_eq!(
            resolve_auth_policy(false, &env, &settings()),
            AuthPolicy::BiometricsOnly
        );
    }

    #[test]
    fn auth_policy_settings_selects_biometrics_only() {
        let mut s = settings();
        s.biometrics_only = true;
        assert_eq!(
            resolve_auth_policy(false, &MapEnv::new(), &s),
            AuthPolicy::BiometricsOnly
        );
    }

    // ── resolve_allow_headless ──

    #[test]
    fn allow_headless_defaults_to_false() {
        assert!(!resolve_allow_headless(false, &MapEnv::new(), &settings()));
    }

    #[test]
    fn allow_headless_cli_flag_wins() {
        assert!(resolve_allow_headless(true, &MapEnv::new(), &settings()));
    }

    #[test]
    fn allow_headless_env_wins() {
        let env = MapEnv::new().with(LEASE_ALLOW_HEADLESS_ENV, "yes");
        assert!(resolve_allow_headless(false, &env, &settings()));
    }

    #[test]
    fn allow_headless_settings_wins() {
        let mut s = settings();
        s.allow_headless = true;
        assert!(resolve_allow_headless(false, &MapEnv::new(), &s));
    }
}
