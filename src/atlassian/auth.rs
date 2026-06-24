//! Atlassian credential management.
//!
//! Loads and saves Atlassian Cloud API credentials from/to the
//! `~/.omni-dev/settings.json` file using the existing `env` map.

use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::atlassian::error::AtlassianError;
use crate::utils::settings::Settings;

/// Environment variable / settings key for the Atlassian instance URL.
pub const ATLASSIAN_INSTANCE_URL: &str = "ATLASSIAN_INSTANCE_URL";

/// Environment variable / settings key for the Atlassian user email.
pub const ATLASSIAN_EMAIL: &str = "ATLASSIAN_EMAIL";

/// Environment variable / settings key for the Atlassian API token.
pub const ATLASSIAN_API_TOKEN: &str = "ATLASSIAN_API_TOKEN";

/// Atlassian Cloud credentials.
#[derive(Debug, Clone)]
pub struct AtlassianCredentials {
    /// Instance base URL (e.g., `"https://myorg.atlassian.net"`).
    pub instance_url: String,

    /// User email address.
    pub email: String,

    /// API token.
    pub api_token: String,
}

/// Loads Atlassian credentials from environment variables or settings.json.
///
/// Checks environment variables first, then falls back to the settings file.
pub fn load_credentials() -> Result<AtlassianCredentials> {
    load_credentials_with_instance(None)
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
    let settings = Settings::load().unwrap_or(Settings {
        env: HashMap::new(),
    });

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
        api_token,
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
    let settings = Settings::load().unwrap_or(Settings {
        env: HashMap::new(),
    });

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
/// the `env` map, and writes back. Preserves all other settings.
pub fn save_credentials(credentials: &AtlassianCredentials) -> Result<()> {
    let settings_path = Settings::get_settings_path()?;

    // Read existing settings as a generic JSON value to preserve unknown fields
    let mut settings_value: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)
            .with_context(|| format!("Failed to read {}", settings_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", settings_path.display()))?
    } else {
        serde_json::json!({})
    };

    // Ensure the "env" key exists as an object
    if !settings_value
        .get("env")
        .is_some_and(serde_json::Value::is_object)
    {
        settings_value["env"] = serde_json::json!({});
    }

    // Merge credential keys — safe because we just ensured "env" is an object above
    let Some(env) = settings_value["env"].as_object_mut() else {
        anyhow::bail!("Internal error: env key is not an object after initialization");
    };
    env.insert(
        ATLASSIAN_INSTANCE_URL.to_string(),
        serde_json::Value::String(credentials.instance_url.clone()),
    );
    env.insert(
        ATLASSIAN_EMAIL.to_string(),
        serde_json::Value::String(credentials.email.clone()),
    );
    env.insert(
        ATLASSIAN_API_TOKEN.to_string(),
        serde_json::Value::String(credentials.api_token.clone()),
    );

    // Ensure parent directory exists
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    // Write back
    let formatted = serde_json::to_string_pretty(&settings_value)
        .context("Failed to serialize settings JSON")?;
    fs::write(&settings_path, formatted)
        .with_context(|| format!("Failed to write {}", settings_path.display()))?;

    Ok(())
}

/// Crate-internal test utilities for code that calls [`load_credentials`] /
/// [`crate::cli::atlassian::helpers::create_client`]. Lives here because it
/// needs the credential constants and shares process-wide env state with
/// auth.rs's own tests — every consumer must serialise on
/// [`AUTH_ENV_MUTEX`] to avoid racing.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod test_util {
    use super::{ATLASSIAN_API_TOKEN, ATLASSIAN_EMAIL, ATLASSIAN_INSTANCE_URL};

    /// Mutex shared by every test that mutates `HOME` and the Atlassian
    /// credential env vars. Serialises those tests against each other so
    /// parallel execution doesn't race on process-wide env state.
    pub(crate) static AUTH_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard: snapshots `HOME` + every Atlassian credential env var on
    /// construction and restores them on drop. Concentrating the save/restore
    /// branches into one place (here) instead of inlining them in each test
    /// keeps coverage high — every test exercises the same guard drop path.
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
                ATLASSIAN_INSTANCE_URL,
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

        /// Clears the three Atlassian credential env vars and points `HOME`
        /// at a fresh empty tempdir so `load_credentials()` returns
        /// `CredentialsNotFound`. Useful for testing the Err propagation
        /// path of code that calls `create_client()`.
        pub(crate) fn clear_credentials(&self) -> tempfile::TempDir {
            let dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            std::env::set_var("HOME", dir.path());
            std::env::remove_var(ATLASSIAN_INSTANCE_URL);
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
            api_token: "token".to_string(),
        };
        let cloned = creds.clone();
        assert_eq!(cloned.instance_url, creds.instance_url);
        assert_eq!(cloned.email, creds.email);
        assert_eq!(cloned.api_token, creds.api_token);
        // Verify Debug is implemented
        let debug = format!("{creds:?}");
        assert!(debug.contains("AtlassianCredentials"));
    }

    use super::test_util::EnvGuard;

    fn with_empty_home(_guard: &EnvGuard) -> tempfile::TempDir {
        let dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        std::env::set_var("HOME", dir.path());
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

    /// Single test for save_credentials to avoid HOME env var race conditions.
    /// Tests both fresh-file creation and merge-with-existing in sequence.
    #[test]
    fn save_credentials_creates_and_preserves() {
        // Share the mutex with the other env-mutating tests in this module
        // so that setting HOME here doesn't race with `status()` tests.
        let _guard = EnvGuard::take();
        let original_home = std::env::var("HOME").ok();

        // ── Part 1: creates file from scratch ──────────────────────
        {
            let temp_dir = {
                std::fs::create_dir_all("tmp").ok();
                tempfile::TempDir::new_in("tmp").unwrap()
            };
            std::env::set_var("HOME", temp_dir.path());

            let creds = AtlassianCredentials {
                instance_url: "https://save.atlassian.net".to_string(),
                email: "save@example.com".to_string(),
                api_token: "save-token".to_string(),
            };
            save_credentials(&creds).unwrap();

            let settings_path = temp_dir.path().join(".omni-dev").join("settings.json");
            assert!(settings_path.exists());
            let content = fs::read_to_string(&settings_path).unwrap();
            let val: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(
                val["env"]["ATLASSIAN_INSTANCE_URL"],
                "https://save.atlassian.net"
            );
            assert_eq!(val["env"]["ATLASSIAN_EMAIL"], "save@example.com");
            assert_eq!(val["env"]["ATLASSIAN_API_TOKEN"], "save-token");
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

            std::env::set_var("HOME", temp_dir.path());

            let creds = AtlassianCredentials {
                instance_url: "https://org.atlassian.net".to_string(),
                email: "user@test.com".to_string(),
                api_token: "token".to_string(),
            };
            save_credentials(&creds).unwrap();

            let content = fs::read_to_string(&settings_path).unwrap();
            let val: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(val["env"]["OTHER_KEY"], "keep_me");
            assert_eq!(val["extra"], true);
            assert_eq!(
                val["env"]["ATLASSIAN_INSTANCE_URL"],
                "https://org.atlassian.net"
            );
        }

        // Restore HOME
        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        }
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
        assert_eq!(creds.api_token, "token");
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
}
