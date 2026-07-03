//! Datadog credential management.
//!
//! Loads and saves Datadog API credentials from/to the
//! `~/.omni-dev/settings.json` file — the active profile's `env` map when a
//! profile is selected, the base `env` map otherwise (issue #1116).

use anyhow::Result;
use serde::Serialize;

use crate::datadog::error::DatadogError;
use crate::utils::env::SystemEnv;
use crate::utils::secret::Secret;
use crate::utils::settings::{active_profile_from, Settings};

/// Environment variable / settings key for the Datadog API key.
pub const DATADOG_API_KEY: &str = "DATADOG_API_KEY";

/// Environment variable / settings key for the Datadog application key.
pub const DATADOG_APP_KEY: &str = "DATADOG_APP_KEY";

/// Environment variable / settings key for the Datadog site (e.g. `datadoghq.com`).
pub const DATADOG_SITE: &str = "DATADOG_SITE";

/// Environment variable / settings key for an explicit Datadog API base URL.
///
/// When set, overrides [`DATADOG_SITE`] entirely — the client uses this URL
/// verbatim instead of deriving `https://api.{site}`. Useful for:
/// - Tests that point at a wiremock server (e.g. `http://127.0.0.1:PORT`).
/// - On-prem / proxied Datadog installs that don't match `api.{site}`.
pub const DATADOG_API_URL: &str = "DATADOG_API_URL";

/// Default Datadog site when none is configured (US1 region).
pub const DEFAULT_SITE: &str = "datadoghq.com";

/// Datadog sites recognised as non-warning.
///
/// Any other value is accepted but logs a warning on [`load_credentials`] —
/// Datadog adds regions occasionally and rejecting unknown values would
/// break the CLI on each new region.
pub const KNOWN_SITES: &[&str] = &[
    "datadoghq.com",
    "us3.datadoghq.com",
    "us5.datadoghq.com",
    "datadoghq.eu",
    "ap1.datadoghq.com",
    "ddog-gov.com",
];

/// Datadog API credentials.
#[derive(Debug, Clone)]
pub struct DatadogCredentials {
    /// API key (organisation-scoped secret; redacted in `Debug` output).
    pub api_key: Secret,

    /// Application key (user-scoped secret; redacted in `Debug` output).
    pub app_key: Secret,

    /// Site identifier, e.g. `datadoghq.com`. Determines the base URL.
    pub site: String,
}

/// Normalises a user-supplied site string.
///
/// Strips any `https://` or `http://` scheme, any `api.` subdomain prefix
/// (users sometimes paste the full API host), and trailing slashes.
pub fn normalize_site(raw: &str) -> String {
    let trimmed = raw.trim();
    let no_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let no_api = no_scheme.strip_prefix("api.").unwrap_or(no_scheme);
    no_api.trim_end_matches('/').to_string()
}

/// Returns the Datadog API base URL for a given site.
pub fn base_url_for_site(site: &str) -> String {
    format!("https://api.{}", normalize_site(site))
}

/// Loads Datadog credentials from environment variables or settings.json.
///
/// Environment variables take precedence over the settings file. Emits a
/// warning on stderr when the configured site is not in [`KNOWN_SITES`].
pub fn load_credentials() -> Result<DatadogCredentials> {
    load_credentials_with(&crate::utils::settings::SettingsEnv::load())
}

/// [`load_credentials`] over an injected [`EnvSource`](crate::utils::env::EnvSource).
///
/// The production wrapper passes `&SettingsEnv::load()` (process env with a
/// settings.json fallback); tests pass a pure `MapEnv`, so credential
/// resolution is exercised without mutating the process environment or `HOME`
/// (issue #1030).
pub(crate) fn load_credentials_with(
    env: &impl crate::utils::env::EnvSource,
) -> Result<DatadogCredentials> {
    let api_key = env
        .var(DATADOG_API_KEY)
        .ok_or(DatadogError::CredentialsNotFound)?;
    let app_key = env
        .var(DATADOG_APP_KEY)
        .ok_or(DatadogError::CredentialsNotFound)?;
    let site = env
        .var(DATADOG_SITE)
        .map(|s| normalize_site(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SITE.to_string());

    if !KNOWN_SITES.iter().any(|k| *k == site) {
        eprintln!("warning: Datadog site '{site}' is not a known region; proceeding anyway");
    }

    Ok(DatadogCredentials {
        api_key: api_key.into(),
        app_key: app_key.into(),
        site,
    })
}

/// Summary of a single Datadog credential scope.
///
/// Reports which credential keys are present without exposing their values.
/// Safe to serialise and return over the MCP surface.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DatadogScopeStatus {
    /// Scope name (currently always `"default"`; forward-compatible for
    /// per-instance scopes).
    pub name: String,
    /// Whether [`DATADOG_API_KEY`] is present.
    pub has_api_key: bool,
    /// Whether [`DATADOG_APP_KEY`] is present.
    pub has_app_key: bool,
    /// Configured site (non-secret; normalised). `None` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

/// Aggregate credential status across every known Datadog scope.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AuthStatus {
    /// One entry per scope. Currently a single default scope; kept as a list
    /// so future multi-instance support does not require a schema change.
    pub scopes: Vec<DatadogScopeStatus>,
}

/// Builds an [`AuthStatus`] from the current settings / environment.
///
/// Reports credential presence without leaking any secret values. Safe to
/// call with no credentials configured.
pub fn status() -> AuthStatus {
    status_with(&crate::utils::settings::SettingsEnv::load())
}

/// [`status`] over an injected [`EnvSource`](crate::utils::env::EnvSource).
///
/// Tests pass a pure `MapEnv` to report presence without mutating the process
/// environment or `HOME` (issue #1030).
pub(crate) fn status_with(env: &impl crate::utils::env::EnvSource) -> AuthStatus {
    let has_api_key = env.var(DATADOG_API_KEY).is_some();
    let has_app_key = env.var(DATADOG_APP_KEY).is_some();
    let site = env
        .var(DATADOG_SITE)
        .map(|s| normalize_site(&s))
        .filter(|s| !s.is_empty());

    AuthStatus {
        scopes: vec![DatadogScopeStatus {
            name: "default".to_string(),
            has_api_key,
            has_app_key,
            site,
        }],
    }
}

/// Saves Datadog credentials to `~/.omni-dev/settings.json`.
///
/// Reads the existing settings file, merges the three credential keys into
/// the active profile's `env` map (the base `env` when no profile is active
/// — issue #1116), and writes back. Preserves all other settings.
pub fn save_credentials(credentials: &DatadogCredentials) -> Result<()> {
    save_credentials_to(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
        credentials,
    )
}

/// [`save_credentials`], writing to an explicit settings-file path and env
/// map (`profiles.<name>.env` when `profile` is `Some`, base `env` otherwise).
///
/// Tests inject a tempdir path and an explicit profile instead of mutating
/// `HOME` / `OMNI_DEV_PROFILE` (issue #1030).
pub(crate) fn save_credentials_to(
    settings_path: &std::path::Path,
    profile: Option<&str>,
    credentials: &DatadogCredentials,
) -> Result<()> {
    Settings::upsert_env_vars_in(
        settings_path,
        profile,
        &[
            (DATADOG_API_KEY, credentials.api_key.expose_secret()),
            (DATADOG_APP_KEY, credentials.app_key.expose_secret()),
            (DATADOG_SITE, credentials.site.as_str()),
        ],
    )
}

/// Removes Datadog credential keys from `~/.omni-dev/settings.json` — from
/// the active profile's `env` map when a profile is active, the base `env`
/// otherwise (issue #1116).
///
/// Leaves all other settings intact. Returns `true` if any Datadog key was
/// present and removed, `false` when the targeted map was already free of
/// them (or the file did not exist).
pub fn remove_credentials() -> Result<bool> {
    remove_credentials_at(
        &Settings::get_settings_path()?,
        active_profile_from(&SystemEnv).as_deref(),
    )
}

/// [`remove_credentials`], operating on an explicit settings-file path and
/// env map (`profiles.<name>.env` when `profile` is `Some`, base `env`
/// otherwise).
///
/// Tests inject a tempdir path and an explicit profile instead of mutating
/// `HOME` / `OMNI_DEV_PROFILE` (issue #1030).
pub(crate) fn remove_credentials_at(
    settings_path: &std::path::Path,
    profile: Option<&str>,
) -> Result<bool> {
    Settings::remove_env_vars_in(
        settings_path,
        profile,
        &[DATADOG_API_KEY, DATADOG_APP_KEY, DATADOG_SITE],
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use super::*;

    // ── Pure helpers (safe to run in parallel) ─────────────────────────

    #[test]
    fn normalize_site_strips_scheme_and_api_prefix() {
        assert_eq!(normalize_site("datadoghq.com"), "datadoghq.com");
        assert_eq!(normalize_site("https://datadoghq.com"), "datadoghq.com");
        assert_eq!(normalize_site("http://datadoghq.com"), "datadoghq.com");
        assert_eq!(normalize_site("api.datadoghq.com"), "datadoghq.com");
        assert_eq!(normalize_site("https://api.datadoghq.com"), "datadoghq.com");
        assert_eq!(
            normalize_site("https://api.us3.datadoghq.com/"),
            "us3.datadoghq.com"
        );
    }

    #[test]
    fn normalize_site_trims_whitespace() {
        assert_eq!(normalize_site("  datadoghq.com  "), "datadoghq.com");
    }

    #[test]
    fn base_url_for_site_builds_api_host() {
        assert_eq!(
            base_url_for_site("datadoghq.com"),
            "https://api.datadoghq.com"
        );
        assert_eq!(
            base_url_for_site("us5.datadoghq.com"),
            "https://api.us5.datadoghq.com"
        );
        assert_eq!(
            base_url_for_site("datadoghq.eu"),
            "https://api.datadoghq.eu"
        );
    }

    #[test]
    fn base_url_normalises_input() {
        assert_eq!(
            base_url_for_site("https://api.datadoghq.com/"),
            "https://api.datadoghq.com"
        );
    }

    #[test]
    fn credentials_struct_clone_and_debug() {
        let creds = DatadogCredentials {
            api_key: "sekret-api-key-value".into(),
            app_key: "sekret-app-key-value".into(),
            site: "datadoghq.com".to_string(),
        };
        let cloned = creds.clone();
        assert_eq!(cloned.api_key, creds.api_key);
        // Debug must never print the key values (#1131).
        let debug = format!("{creds:?}");
        assert!(debug.contains("DatadogCredentials"));
        assert!(
            !debug.contains("sekret-api-key-value"),
            "leaked api_key: {debug}"
        );
        assert!(
            !debug.contains("sekret-app-key-value"),
            "leaked app_key: {debug}"
        );
        assert!(debug.contains("api_key: <redacted>"));
        assert!(debug.contains("app_key: <redacted>"));
    }

    #[test]
    fn constant_key_names() {
        assert_eq!(DATADOG_API_KEY, "DATADOG_API_KEY");
        assert_eq!(DATADOG_APP_KEY, "DATADOG_APP_KEY");
        assert_eq!(DATADOG_SITE, "DATADOG_SITE");
        assert_eq!(DEFAULT_SITE, "datadoghq.com");
    }

    #[test]
    fn known_sites_contains_common_regions() {
        assert!(KNOWN_SITES.contains(&"datadoghq.com"));
        assert!(KNOWN_SITES.contains(&"datadoghq.eu"));
        assert!(KNOWN_SITES.contains(&"us5.datadoghq.com"));
    }

    // ── Env-parsing boundary tests (injected, no process-env mutation) ──

    use crate::test_support::env::MapEnv;

    #[test]
    fn status_reports_all_false_when_nothing_configured() {
        let status = status_with(&MapEnv::new());
        assert_eq!(status.scopes.len(), 1);
        let scope = &status.scopes[0];
        assert_eq!(scope.name, "default");
        assert!(!scope.has_api_key);
        assert!(!scope.has_app_key);
        assert_eq!(scope.site, None);
    }

    #[test]
    fn status_reports_presence_flags_without_leaking_secrets() {
        let env = MapEnv::new()
            .with(DATADOG_API_KEY, "sekret-api-do-not-leak")
            .with(DATADOG_APP_KEY, "sekret-app-do-not-leak")
            .with(DATADOG_SITE, "datadoghq.com");

        let status = status_with(&env);
        let scope = &status.scopes[0];
        assert!(scope.has_api_key);
        assert!(scope.has_app_key);
        assert_eq!(scope.site.as_deref(), Some("datadoghq.com"));

        let yaml = serde_yaml::to_string(&status).unwrap();
        assert!(!yaml.contains("sekret-api-do-not-leak"));
        assert!(!yaml.contains("sekret-app-do-not-leak"));
    }

    #[test]
    fn status_normalises_site_value() {
        let env = MapEnv::new().with(DATADOG_SITE, "https://api.us3.datadoghq.com/");
        let status = status_with(&env);
        assert_eq!(status.scopes[0].site.as_deref(), Some("us3.datadoghq.com"));
    }

    #[test]
    fn load_credentials_errors_when_api_key_missing() {
        let env = MapEnv::new().with(DATADOG_APP_KEY, "app");
        let err = load_credentials_with(&env).unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn load_credentials_defaults_site_when_unset() {
        let env = MapEnv::new()
            .with(DATADOG_API_KEY, "api")
            .with(DATADOG_APP_KEY, "app");
        let creds = load_credentials_with(&env).unwrap();
        assert_eq!(creds.site, DEFAULT_SITE);
    }

    #[test]
    fn load_credentials_warns_on_unknown_site_but_succeeds() {
        let env = MapEnv::new()
            .with(DATADOG_API_KEY, "api")
            .with(DATADOG_APP_KEY, "app")
            .with(DATADOG_SITE, "custom.example");
        let creds = load_credentials_with(&env).unwrap();
        assert_eq!(creds.site, "custom.example");
    }

    /// Save + remove round-trip against injected settings-file paths — no
    /// `HOME` mutation, so the test needs no lock. Covers fresh-file creation,
    /// merge-with-existing, and removal.
    #[test]
    fn save_then_remove_round_trip() {
        // ── Part 1: creates file from scratch ──────────────────────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let settings_path = temp_dir.path().join(".omni-dev").join("settings.json");

            let creds = DatadogCredentials {
                api_key: "api-1".into(),
                app_key: "app-1".into(),
                site: "datadoghq.com".to_string(),
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            assert!(settings_path.exists());
            let content = fs::read_to_string(&settings_path).unwrap();
            let val: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(val["env"]["DATADOG_API_KEY"], "api-1");
            assert_eq!(val["env"]["DATADOG_APP_KEY"], "app-1");
            assert_eq!(val["env"]["DATADOG_SITE"], "datadoghq.com");

            // The credential store is created owner-only (issue #1128).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&settings_path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }

        // ── Part 2: merges into existing settings ──────────────────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let omni_dir = temp_dir.path().join(".omni-dev");
            fs::create_dir_all(&omni_dir).unwrap();
            let settings_path = omni_dir.join("settings.json");
            fs::write(
                &settings_path,
                r#"{"env": {"OTHER_KEY": "keep_me"}, "extra": true}"#,
            )
            .unwrap();

            let creds = DatadogCredentials {
                api_key: "api-2".into(),
                app_key: "app-2".into(),
                site: "datadoghq.eu".to_string(),
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert_eq!(val["env"]["OTHER_KEY"], "keep_me");
            assert_eq!(val["extra"], true);
            assert_eq!(val["env"]["DATADOG_SITE"], "datadoghq.eu");
        }

        // ── Part 3: remove clears the three keys, preserves others ─
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let omni_dir = temp_dir.path().join(".omni-dev");
            fs::create_dir_all(&omni_dir).unwrap();
            let settings_path = omni_dir.join("settings.json");
            fs::write(
                &settings_path,
                r#"{"env": {
                    "DATADOG_API_KEY": "a",
                    "DATADOG_APP_KEY": "b",
                    "DATADOG_SITE": "datadoghq.com",
                    "OTHER_KEY": "keep"
                }}"#,
            )
            .unwrap();

            let removed = remove_credentials_at(&settings_path, None).unwrap();
            assert!(removed);

            let val: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
            assert!(val["env"].get("DATADOG_API_KEY").is_none());
            assert!(val["env"].get("DATADOG_APP_KEY").is_none());
            assert!(val["env"].get("DATADOG_SITE").is_none());
            assert_eq!(val["env"]["OTHER_KEY"], "keep");
        }

        // ── Part 4: remove returns false when nothing to remove ────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let settings_path = temp_dir.path().join(".omni-dev").join("settings.json");
            let removed = remove_credentials_at(&settings_path, None).unwrap();
            assert!(!removed);
        }
    }

    /// Save + remove round-trip against a profile-targeted env map — the
    /// credentials land under `profiles.<name>.env` where the profile-aware
    /// read side will find them, and the base `env` is untouched
    /// (issue #1116).
    #[test]
    fn save_then_remove_round_trip_in_profile() {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        let omni_dir = temp_dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        let settings_path = omni_dir.join("settings.json");
        fs::write(&settings_path, r#"{"env": {"OTHER_KEY": "keep_me"}}"#).unwrap();

        let creds = DatadogCredentials {
            api_key: "api-p".into(),
            app_key: "app-p".into(),
            site: "datadoghq.eu".to_string(),
        };
        save_credentials_to(&settings_path, Some("work"), &creds).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["profiles"]["work"]["env"]["DATADOG_API_KEY"], "api-p");
        assert_eq!(
            val["profiles"]["work"]["env"]["DATADOG_SITE"],
            "datadoghq.eu"
        );
        assert!(val["env"].get("DATADOG_API_KEY").is_none());
        assert_eq!(val["env"]["OTHER_KEY"], "keep_me");

        let removed = remove_credentials_at(&settings_path, Some("work")).unwrap();
        assert!(removed);
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["profiles"]["work"]["env"]
            .get("DATADOG_API_KEY")
            .is_none());

        let removed = remove_credentials_at(&settings_path, Some("work")).unwrap();
        assert!(!removed);
    }

    /// The production wrappers resolve `~/.omni-dev/settings.json` from
    /// `HOME` and the active profile from `OMNI_DEV_PROFILE`, so this one
    /// test must redirect both (under the shared
    /// [`crate::atlassian::auth::test_util::EnvGuard`], which serialises all
    /// env-mutating auth tests). Every other save/remove test injects a path
    /// and profile instead (issue #1030).
    #[test]
    fn save_and_remove_credentials_resolve_default_settings_path() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let dir = guard.clear_credentials();

        let creds = DatadogCredentials {
            api_key: "wrapper-api".into(),
            app_key: "wrapper-app".into(),
            site: "datadoghq.com".to_string(),
        };
        save_credentials(&creds).unwrap();

        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["env"]["DATADOG_API_KEY"], "wrapper-api");

        assert!(remove_credentials().unwrap());
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get("DATADOG_API_KEY").is_none());
    }

    /// A profile-targeted logout must not clear base-`env` credentials
    /// belonging to the default bundle (issue #1116).
    #[test]
    fn remove_credentials_at_profile_ignores_base_keys() {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        let omni_dir = temp_dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        let settings_path = omni_dir.join("settings.json");
        fs::write(
            &settings_path,
            r#"{"env": {"DATADOG_API_KEY": "base-a", "DATADOG_APP_KEY": "base-b"}}"#,
        )
        .unwrap();

        let removed = remove_credentials_at(&settings_path, Some("work")).unwrap();
        assert!(!removed);

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["env"]["DATADOG_API_KEY"], "base-a");
        assert_eq!(val["env"]["DATADOG_APP_KEY"], "base-b");
    }
}
