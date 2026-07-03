//! Datadog credential management.
//!
//! Loads and saves Datadog API credentials from/to the
//! `~/.omni-dev/settings.json` file using the existing `env` map.

use std::fs;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::datadog::error::DatadogError;
use crate::utils::secret::Secret;
use crate::utils::settings::Settings;

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
/// the `env` map, and writes back. Preserves all other settings.
pub fn save_credentials(credentials: &DatadogCredentials) -> Result<()> {
    save_credentials_to(&Settings::get_settings_path()?, credentials)
}

/// [`save_credentials`], writing to an explicit settings-file path.
///
/// Tests inject a tempdir path instead of redirecting `HOME` (issue #1030).
pub(crate) fn save_credentials_to(
    settings_path: &std::path::Path,
    credentials: &DatadogCredentials,
) -> Result<()> {
    let mut settings_value = read_or_default_settings(settings_path)?;
    ensure_env_object(&mut settings_value);

    let Some(env) = settings_value["env"].as_object_mut() else {
        anyhow::bail!("Internal error: env key is not an object after initialization");
    };
    env.insert(
        DATADOG_API_KEY.to_string(),
        serde_json::Value::String(credentials.api_key.expose_secret().to_string()),
    );
    env.insert(
        DATADOG_APP_KEY.to_string(),
        serde_json::Value::String(credentials.app_key.expose_secret().to_string()),
    );
    env.insert(
        DATADOG_SITE.to_string(),
        serde_json::Value::String(credentials.site.clone()),
    );

    write_settings(settings_path, &settings_value)
}

/// Removes Datadog credential keys from `~/.omni-dev/settings.json`.
///
/// Leaves all other settings intact. Returns `true` if any Datadog key was
/// present and removed, `false` when the file was already free of them (or
/// did not exist).
pub fn remove_credentials() -> Result<bool> {
    remove_credentials_at(&Settings::get_settings_path()?)
}

/// [`remove_credentials`], operating on an explicit settings-file path.
///
/// Tests inject a tempdir path instead of redirecting `HOME` (issue #1030).
pub(crate) fn remove_credentials_at(settings_path: &std::path::Path) -> Result<bool> {
    if !settings_path.exists() {
        return Ok(false);
    }
    let mut settings_value = read_or_default_settings(settings_path)?;

    let mut removed = false;
    if let Some(env) = settings_value
        .get_mut("env")
        .and_then(serde_json::Value::as_object_mut)
    {
        for key in [DATADOG_API_KEY, DATADOG_APP_KEY, DATADOG_SITE] {
            if env.remove(key).is_some() {
                removed = true;
            }
        }
    }

    if removed {
        write_settings(settings_path, &settings_value)?;
    }
    Ok(removed)
}

fn read_or_default_settings(path: &std::path::Path) -> Result<serde_json::Value> {
    if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))
    } else {
        Ok(serde_json::json!({}))
    }
}

fn ensure_env_object(value: &mut serde_json::Value) {
    if !value.get("env").is_some_and(serde_json::Value::is_object) {
        value["env"] = serde_json::json!({});
    }
}

fn write_settings(path: &std::path::Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let formatted =
        serde_json::to_string_pretty(value).context("Failed to serialize settings JSON")?;
    fs::write(path, formatted).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
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
            save_credentials_to(&settings_path, &creds).unwrap();

            assert!(settings_path.exists());
            let content = fs::read_to_string(&settings_path).unwrap();
            let val: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(val["env"]["DATADOG_API_KEY"], "api-1");
            assert_eq!(val["env"]["DATADOG_APP_KEY"], "app-1");
            assert_eq!(val["env"]["DATADOG_SITE"], "datadoghq.com");
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
            save_credentials_to(&settings_path, &creds).unwrap();

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

            let removed = remove_credentials_at(&settings_path).unwrap();
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
            let removed = remove_credentials_at(&settings_path).unwrap();
            assert!(!removed);
        }
    }
}
