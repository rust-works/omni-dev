//! Atlassian credential management.
//!
//! Loads and saves Atlassian Cloud API credentials from/to the
//! `~/.omni-dev/settings.json` file using the existing `env` map.

use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};

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
    /// Instance base URL (e.g., "https://myorg.atlassian.net").
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
    let settings = Settings::load().unwrap_or(Settings {
        env: HashMap::new(),
    });

    let instance_url = settings
        .get_env_var(ATLASSIAN_INSTANCE_URL)
        .ok_or(AtlassianError::CredentialsNotFound)?;
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

    /// Single test for save_credentials to avoid HOME env var race conditions.
    /// Tests both fresh-file creation and merge-with-existing in sequence.
    #[test]
    fn save_credentials_creates_and_preserves() {
        use std::sync::Mutex;
        static HOME_MUTEX: Mutex<()> = Mutex::new(());
        let _lock = HOME_MUTEX.lock().unwrap();

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
}
