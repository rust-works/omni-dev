//! Atlassian credential management.
//!
//! Loads and saves Atlassian Cloud API credentials from/to the
//! `~/.omni-dev/settings.json` file — the active profile's `env` map when a
//! profile is selected, the base `env` map otherwise (issue #1116).

use anyhow::Result;
use serde::Serialize;

use crate::atlassian::error::AtlassianError;
use crate::utils::env::SystemEnv;
use crate::utils::secret::Secret;
use crate::utils::settings::{active_profile_from, Settings};

/// Environment variable / settings key for the Atlassian instance URL.
pub const ATLASSIAN_INSTANCE_URL: &str = "ATLASSIAN_INSTANCE_URL";

/// Environment variable / settings key for the Atlassian user email.
pub const ATLASSIAN_EMAIL: &str = "ATLASSIAN_EMAIL";

/// Environment variable / settings key for the Atlassian API token.
pub const ATLASSIAN_API_TOKEN: &str = "ATLASSIAN_API_TOKEN";

/// Environment variable that overrides the Atlassian instance URL.
///
/// Applies to **every** JIRA/Confluence command. Set by the global `--instance`
/// flag via [`crate::cli::Cli`]'s `propagate_global_flags`; takes precedence
/// over `ATLASSIAN_INSTANCE_URL` / settings.json. A caller-supplied override
/// (e.g. via [`load_credentials_with_instance`]) still wins over it.
pub const ATLASSIAN_INSTANCE_OVERRIDE_ENV: &str = "OMNI_DEV_ATLASSIAN_INSTANCE";

/// Atlassian Cloud credentials.
#[derive(Debug, Clone)]
pub struct AtlassianCredentials {
    /// Instance base URL (e.g., `"https://myorg.atlassian.net"`).
    pub instance_url: String,

    /// User email address.
    pub email: String,

    /// API token (secret; redacted in `Debug` output).
    pub api_token: Secret,
}

/// Loads Atlassian credentials from environment variables or settings.json.
///
/// Checks environment variables first, then falls back to the settings file.
/// The global `--instance` flag (propagated to [`ATLASSIAN_INSTANCE_OVERRIDE_ENV`])
/// overrides the configured instance URL for every command; a blank value is
/// ignored. Callers that carry their own explicit override should call
/// [`load_credentials_with_instance`] directly.
pub fn load_credentials() -> Result<AtlassianCredentials> {
    let env_override = std::env::var(ATLASSIAN_INSTANCE_OVERRIDE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty());
    load_credentials_with_instance(env_override.as_deref())
}

/// Loads Atlassian credentials, optionally overriding the instance URL.
///
/// When `instance_override` is `Some`, that URL is used verbatim (after
/// trailing-slash normalization) and the `ATLASSIAN_INSTANCE_URL` env /
/// settings lookup is skipped — so a caller-supplied instance (e.g.
/// `jira create --instance`) works even when no instance is configured in the
/// environment. `ATLASSIAN_EMAIL` and `ATLASSIAN_API_TOKEN` are still required.
/// When `None`, behaves exactly like [`load_credentials`].
pub fn load_credentials_with_instance(
    instance_override: Option<&str>,
) -> Result<AtlassianCredentials> {
    let settings = Settings::load().unwrap_or_default();

    let instance_url = match instance_override {
        Some(url) => url.to_string(),
        None => settings
            .get_env_var(ATLASSIAN_INSTANCE_URL)
            .ok_or(AtlassianError::CredentialsNotFound)?,
    };
    let email = settings
        .get_env_var(ATLASSIAN_EMAIL)
        .ok_or(AtlassianError::CredentialsNotFound)?;
    let api_token = settings
        .get_env_var(ATLASSIAN_API_TOKEN)
        .ok_or(AtlassianError::CredentialsNotFound)?;

    // Normalize: strip trailing slash from instance URL
    let instance_url = instance_url.trim_end_matches('/').to_string();

    Ok(AtlassianCredentials {
        instance_url,
        email,
        api_token: api_token.into(),
    })
}

/// Summary of a single Atlassian credential scope.
///
/// Reports which credential keys are present without exposing their values.
/// Safe to serialize and return over the MCP surface.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AtlassianScopeStatus {
    /// Scope name (currently always `"default"`; forward-compatible for
    /// per-instance scopes).
    pub name: String,
    /// Whether [`ATLASSIAN_EMAIL`] is present.
    pub has_email: bool,
    /// Whether [`ATLASSIAN_API_TOKEN`] is present. Token value is never exposed.
    pub has_token: bool,
    /// Value of [`ATLASSIAN_INSTANCE_URL`] when set. The URL is considered
    /// non-secret; returning it helps the assistant surface which instance
    /// a scope targets without exposing credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_url: Option<String>,
}

/// Aggregate credential status across every known Atlassian scope.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AuthStatus {
    /// One entry per scope. Currently a single default scope; kept as a list
    /// so future multi-instance support does not require a schema change.
    pub scopes: Vec<AtlassianScopeStatus>,
}

/// Builds an [`AuthStatus`] from the current settings / environment.
///
/// Reports credential presence without leaking any secret values.
/// [`AtlassianScopeStatus::instance_url`] is returned verbatim when set —
/// URLs are explicitly non-secret; tokens and emails are flagged as booleans
/// only. Safe to call with no credentials configured (returns a scope with
/// every flag `false`).
pub fn status() -> AuthStatus {
    let settings = Settings::load().unwrap_or_default();

    let instance_url = settings
        .get_env_var(ATLASSIAN_INSTANCE_URL)
        .map(|v| v.trim_end_matches('/').to_string());
    let has_email = settings.get_env_var(ATLASSIAN_EMAIL).is_some();
    let has_token = settings.get_env_var(ATLASSIAN_API_TOKEN).is_some();

    AuthStatus {
        scopes: vec![AtlassianScopeStatus {
            name: "default".to_string(),
            has_email,
            has_token,
            instance_url,
        }],
    }
}

/// Saves Atlassian credentials to `~/.omni-dev/settings.json`.
///
/// Reads the existing settings file, merges the new credential keys into
/// the active profile's `env` map (the base `env` when no profile is active
/// — issue #1116), and writes back. Preserves all other settings.
pub fn save_credentials(credentials: &AtlassianCredentials) -> Result<()> {
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
    credentials: &AtlassianCredentials,
) -> Result<()> {
    Settings::upsert_env_vars_in(
        settings_path,
        profile,
        &[
            (ATLASSIAN_INSTANCE_URL, credentials.instance_url.as_str()),
            (ATLASSIAN_EMAIL, credentials.email.as_str()),
            (ATLASSIAN_API_TOKEN, credentials.api_token.expose_secret()),
        ],
    )
}

/// Removes Atlassian credential keys from `~/.omni-dev/settings.json` — from
/// the active profile's `env` map when a profile is active, the base `env`
/// otherwise (issue #1116).
///
/// Leaves all other settings intact. Returns `true` if any Atlassian key was
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
        &[ATLASSIAN_INSTANCE_URL, ATLASSIAN_EMAIL, ATLASSIAN_API_TOKEN],
    )
}

/// Crate-internal test utilities for code that calls [`load_credentials`] /
/// [`crate::cli::atlassian::helpers::create_client`]. Lives here because it
/// needs the credential constants and shares process-wide env state with
/// auth.rs's own tests — every consumer must serialise on
/// [`AUTH_ENV_MUTEX`] to avoid racing.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod test_util {
    use super::{
        ATLASSIAN_API_TOKEN, ATLASSIAN_EMAIL, ATLASSIAN_INSTANCE_OVERRIDE_ENV,
        ATLASSIAN_INSTANCE_URL,
    };
    use crate::utils::settings::PROFILE_ENV_VAR;

    /// Mutex shared by every test that mutates `HOME`, `OMNI_DEV_PROFILE`, or
    /// the Atlassian credential env vars. Serialises those tests against each
    /// other so parallel execution doesn't race on process-wide env state.
    pub(crate) static AUTH_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard: snapshots `HOME`, `OMNI_DEV_PROFILE`, and every Atlassian
    /// credential env var on construction and restores them on drop.
    /// Concentrating the save/restore branches into one place (here) instead
    /// of inlining them in each test keeps coverage high — every test
    /// exercises the same guard drop path.
    pub(crate) struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        snapshot: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        pub(crate) fn take() -> Self {
            let lock = AUTH_ENV_MUTEX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let keys = [
                "HOME",
                PROFILE_ENV_VAR,
                ATLASSIAN_INSTANCE_URL,
                ATLASSIAN_INSTANCE_OVERRIDE_ENV,
                ATLASSIAN_EMAIL,
                ATLASSIAN_API_TOKEN,
            ];
            let snapshot = keys
                .into_iter()
                .map(|k| (k, std::env::var(k).ok()))
                .collect();
            Self {
                _lock: lock,
                snapshot,
            }
        }

        /// Clears the three Atlassian credential env vars plus
        /// `OMNI_DEV_PROFILE` and points `HOME` at a fresh empty tempdir so
        /// `load_credentials()` returns `CredentialsNotFound` and settings
        /// writes target the base `env` map. Useful for testing the Err
        /// propagation path of code that calls `create_client()` and the
        /// `HOME`-resolving credential-write wrappers.
        pub(crate) fn clear_credentials(&self) -> tempfile::TempDir {
            let dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            std::env::set_var("HOME", dir.path());
            std::env::remove_var(PROFILE_ENV_VAR);
            std::env::remove_var(ATLASSIAN_INSTANCE_URL);
            std::env::remove_var(ATLASSIAN_INSTANCE_OVERRIDE_ENV);
            std::env::remove_var(ATLASSIAN_EMAIL);
            std::env::remove_var(ATLASSIAN_API_TOKEN);
            dir
        }

        /// Sets the three Atlassian credential env vars to point at a wiremock
        /// (or any HTTP) endpoint. The matching `HOME` is replaced with a
        /// fresh tempdir so any `~/.omni-dev/settings.json` on the developer's
        /// machine does not bleed in.
        pub(crate) fn set_credentials(&self, instance_url: &str) -> tempfile::TempDir {
            let dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            std::env::set_var("HOME", dir.path());
            std::env::set_var(ATLASSIAN_INSTANCE_URL, instance_url);
            std::env::set_var(ATLASSIAN_EMAIL, "test@example.com");
            std::env::set_var(ATLASSIAN_API_TOKEN, "test-token");
            dir
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.snapshot {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn save_and_read_credentials() {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        let settings_path = temp_dir.path().join("settings.json");

        // Start with existing settings
        let existing = r#"{"env": {"SOME_KEY": "value"}}"#;
        fs::write(&settings_path, existing).unwrap();

        // Read it back as a value, add credentials, write
        let content = fs::read_to_string(&settings_path).unwrap();
        let mut val: serde_json::Value = serde_json::from_str(&content).unwrap();
        val["env"]["ATLASSIAN_INSTANCE_URL"] =
            serde_json::Value::String("https://test.atlassian.net".to_string());
        val["env"]["ATLASSIAN_EMAIL"] = serde_json::Value::String("user@example.com".to_string());
        val["env"]["ATLASSIAN_API_TOKEN"] = serde_json::Value::String("secret-token".to_string());
        let formatted = serde_json::to_string_pretty(&val).unwrap();
        fs::write(&settings_path, formatted).unwrap();

        // Verify existing keys are preserved
        let content = fs::read_to_string(&settings_path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(val["env"]["SOME_KEY"], "value");
        assert_eq!(
            val["env"]["ATLASSIAN_INSTANCE_URL"],
            "https://test.atlassian.net"
        );
        assert_eq!(val["env"]["ATLASSIAN_EMAIL"], "user@example.com");
        assert_eq!(val["env"]["ATLASSIAN_API_TOKEN"], "secret-token");
    }

    #[test]
    fn load_credentials_normalizes_trailing_slash() {
        // Test the trailing-slash normalization logic directly
        let url = "https://env.atlassian.net/";
        let normalized = url.trim_end_matches('/').to_string();
        assert_eq!(normalized, "https://env.atlassian.net");
    }

    #[test]
    fn constant_key_names() {
        assert_eq!(ATLASSIAN_INSTANCE_URL, "ATLASSIAN_INSTANCE_URL");
        assert_eq!(ATLASSIAN_EMAIL, "ATLASSIAN_EMAIL");
        assert_eq!(ATLASSIAN_API_TOKEN, "ATLASSIAN_API_TOKEN");
    }

    #[test]
    fn credentials_struct_clone_and_debug() {
        let creds = AtlassianCredentials {
            instance_url: "https://org.atlassian.net".to_string(),
            email: "user@test.com".to_string(),
            api_token: "super-sekret-api-token-value".into(),
        };
        let cloned = creds.clone();
        assert_eq!(cloned.instance_url, creds.instance_url);
        assert_eq!(cloned.email, creds.email);
        assert_eq!(cloned.api_token, creds.api_token);
        // Debug must never print the token value (#1131).
        let debug = format!("{creds:?}");
        assert!(debug.contains("AtlassianCredentials"));
        assert!(
            !debug.contains("super-sekret-api-token-value"),
            "leaked token: {debug}"
        );
        assert!(debug.contains("api_token: <redacted>"));
    }

    use super::test_util::EnvGuard;

    fn with_empty_home(_guard: &EnvGuard) -> tempfile::TempDir {
        let dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        std::env::set_var("HOME", dir.path());
        std::env::remove_var(crate::utils::settings::PROFILE_ENV_VAR);
        std::env::remove_var(ATLASSIAN_INSTANCE_URL);
        std::env::remove_var(ATLASSIAN_EMAIL);
        std::env::remove_var(ATLASSIAN_API_TOKEN);
        dir
    }

    #[test]
    fn status_reports_all_false_when_nothing_configured() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let status = status();
        assert_eq!(status.scopes.len(), 1);
        let scope = &status.scopes[0];
        assert_eq!(scope.name, "default");
        assert!(!scope.has_email);
        assert!(!scope.has_token);
        assert_eq!(scope.instance_url, None);
    }

    #[test]
    fn status_reports_presence_flags_from_settings_without_leaking_secrets() {
        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        let omni_dir = dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{
                "ATLASSIAN_INSTANCE_URL":"https://status.atlassian.net/",
                "ATLASSIAN_EMAIL":"person@example.com",
                "ATLASSIAN_API_TOKEN":"sekret-do-not-leak"
            }}"#,
        )
        .unwrap();

        let status = status();
        assert_eq!(status.scopes.len(), 1);
        let scope = &status.scopes[0];
        assert!(scope.has_email);
        assert!(scope.has_token);
        assert_eq!(
            scope.instance_url.as_deref(),
            Some("https://status.atlassian.net")
        );

        let yaml = serde_yaml::to_string(&status).unwrap();
        assert!(!yaml.contains("sekret-do-not-leak"), "leaked token: {yaml}");
        assert!(!yaml.contains("person@example.com"), "leaked email: {yaml}");
    }

    #[test]
    fn status_returns_instance_url_from_env_without_trailing_slash() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        std::env::set_var(ATLASSIAN_INSTANCE_URL, "https://env.atlassian.net/");

        let status = status();
        let scope = &status.scopes[0];
        assert_eq!(
            scope.instance_url.as_deref(),
            Some("https://env.atlassian.net")
        );
        assert!(!scope.has_email);
        assert!(!scope.has_token);
    }

    /// The production wrapper resolves `~/.omni-dev/settings.json` from
    /// `HOME`, which `dirs::home_dir()` reads internally — so this one test
    /// must redirect `HOME` (under the shared [`EnvGuard`]). Every other save
    /// test injects a path into `save_credentials_to` instead (issue #1030).
    #[test]
    fn save_credentials_resolves_default_settings_path() {
        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);

        let creds = AtlassianCredentials {
            instance_url: "https://wrapper.atlassian.net".to_string(),
            email: "wrapper@example.com".to_string(),
            api_token: "wrapper-token".into(),
        };
        save_credentials(&creds).unwrap();

        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["env"]["ATLASSIAN_EMAIL"], "wrapper@example.com");
    }

    /// The `remove_credentials()` wrapper resolves the settings path from
    /// `HOME` and the profile from the environment; every other removal test
    /// injects both into `remove_credentials_at` (issue #1030).
    #[test]
    fn remove_credentials_resolves_default_settings_path() {
        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);

        let creds = AtlassianCredentials {
            instance_url: "https://wrapper.atlassian.net".to_string(),
            email: "wrapper@example.com".to_string(),
            api_token: "wrapper-token".into(),
        };
        save_credentials(&creds).unwrap();

        // Present → removed.
        assert!(remove_credentials().unwrap());

        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(val["env"].get(ATLASSIAN_EMAIL).is_none());
        assert!(val["env"].get(ATLASSIAN_API_TOKEN).is_none());

        // Idempotent: nothing left to remove.
        assert!(!remove_credentials().unwrap());
    }

    /// Save against injected settings-file paths — no `HOME` mutation, so the
    /// test needs no lock (issue #1030). Covers fresh-file creation and
    /// merge-with-existing.
    #[test]
    fn save_credentials_creates_and_preserves() {
        // ── Part 1: creates file from scratch ──────────────────────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            let settings_path = temp_dir.path().join(".omni-dev").join("settings.json");

            let creds = AtlassianCredentials {
                instance_url: "https://save.atlassian.net".to_string(),
                email: "save@example.com".to_string(),
                api_token: "save-token".into(),
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            assert!(settings_path.exists());
            let content = fs::read_to_string(&settings_path).unwrap();
            let val: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(
                val["env"]["ATLASSIAN_INSTANCE_URL"],
                "https://save.atlassian.net"
            );
            assert_eq!(val["env"]["ATLASSIAN_EMAIL"], "save@example.com");
            assert_eq!(val["env"]["ATLASSIAN_API_TOKEN"], "save-token");

            // The credential store is created owner-only (issue #1128).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&settings_path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }

        // ── Part 2: preserves existing keys ────────────────────────
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

            let creds = AtlassianCredentials {
                instance_url: "https://org.atlassian.net".to_string(),
                email: "user@test.com".to_string(),
                api_token: "token".into(),
            };
            save_credentials_to(&settings_path, None, &creds).unwrap();

            let content = fs::read_to_string(&settings_path).unwrap();
            let val: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(val["env"]["OTHER_KEY"], "keep_me");
            assert_eq!(val["extra"], true);
            assert_eq!(
                val["env"]["ATLASSIAN_INSTANCE_URL"],
                "https://org.atlassian.net"
            );
        }
    }

    /// A profile-targeted save lands under `profiles.<name>.env` — where the
    /// profile-aware read side will find it — and leaves the base `env`
    /// untouched (issue #1116).
    #[test]
    fn save_credentials_to_profile_writes_into_profile_env() {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        let omni_dir = temp_dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        let settings_path = omni_dir.join("settings.json");
        fs::write(&settings_path, r#"{"env": {"OTHER_KEY": "keep_me"}}"#).unwrap();

        let creds = AtlassianCredentials {
            instance_url: "https://work.atlassian.net".to_string(),
            email: "work@example.com".to_string(),
            api_token: "work-token".into(),
        };
        save_credentials_to(&settings_path, Some("work"), &creds).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            val["profiles"]["work"]["env"]["ATLASSIAN_EMAIL"],
            "work@example.com"
        );
        assert_eq!(
            val["profiles"]["work"]["env"]["ATLASSIAN_INSTANCE_URL"],
            "https://work.atlassian.net"
        );
        assert!(val["env"].get("ATLASSIAN_EMAIL").is_none());
        assert_eq!(val["env"]["OTHER_KEY"], "keep_me");
    }

    #[test]
    fn load_credentials_with_instance_override_supplies_instance_url() {
        // The override lets a caller (e.g. `jira create --instance`) target an
        // instance even when ATLASSIAN_INSTANCE_URL is unset, as long as email
        // and token are present. The trailing slash is normalized.
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        std::env::set_var(ATLASSIAN_EMAIL, "person@example.com");
        std::env::set_var(ATLASSIAN_API_TOKEN, "token");

        let creds =
            load_credentials_with_instance(Some("https://override.atlassian.net/")).unwrap();
        assert_eq!(creds.instance_url, "https://override.atlassian.net");
        assert_eq!(creds.email, "person@example.com");
        assert_eq!(creds.api_token.expose_secret(), "token");
    }

    #[test]
    fn load_credentials_with_instance_none_requires_env_instance() {
        // Without an override and without ATLASSIAN_INSTANCE_URL configured,
        // resolution fails just like load_credentials() does today.
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        std::env::set_var(ATLASSIAN_EMAIL, "person@example.com");
        std::env::set_var(ATLASSIAN_API_TOKEN, "token");

        assert!(load_credentials_with_instance(None).is_err());
    }

    #[test]
    fn load_credentials_honours_instance_override_env() {
        // The global `--instance` flag (exported as OMNI_DEV_ATLASSIAN_INSTANCE)
        // overrides the instance for every command, even with no configured
        // ATLASSIAN_INSTANCE_URL (#1117). A blank value is ignored.
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        std::env::set_var(ATLASSIAN_EMAIL, "person@example.com");
        std::env::set_var(ATLASSIAN_API_TOKEN, "token");

        // Blank → ignored → falls back to the (absent) configured instance.
        std::env::set_var(ATLASSIAN_INSTANCE_OVERRIDE_ENV, "  ");
        assert!(load_credentials().is_err());

        // Set → used verbatim (trailing slash normalized).
        std::env::set_var(
            ATLASSIAN_INSTANCE_OVERRIDE_ENV,
            "https://flag.atlassian.net/",
        );
        let creds = load_credentials().unwrap();
        assert_eq!(creds.instance_url, "https://flag.atlassian.net");
        assert_eq!(creds.email, "person@example.com");
    }
}
