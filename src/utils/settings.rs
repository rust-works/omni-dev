//! Settings and configuration utilities.
//!
//! This module provides functionality to read settings from $HOME/.omni-dev/settings.json
//! and use them as a fallback for environment variables.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Settings loaded from $HOME/.omni-dev/settings.json.
#[derive(Debug, Deserialize)]
pub struct Settings {
    /// Environment variable overrides.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl Settings {
    /// Loads settings from the default location.
    pub fn load() -> Result<Self> {
        let settings_path = Self::get_settings_path()?;
        Self::load_from_path(&settings_path)
    }

    /// Loads settings from a specific path.
    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        // If file doesn't exist, return default settings
        if !path.exists() {
            return Ok(Settings {
                env: HashMap::new(),
            });
        }

        // Read and parse the settings file
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read settings file: {}", path.display()))?;

        serde_json::from_str::<Settings>(&content)
            .with_context(|| format!("Failed to parse settings file: {}", path.display()))
    }

    /// Returns the default settings path.
    pub fn get_settings_path() -> Result<PathBuf> {
        let home_dir = dirs::home_dir().context("Failed to determine home directory")?;

        Ok(home_dir.join(".omni-dev").join("settings.json"))
    }

    /// Returns an environment variable with fallback to settings.
    pub fn get_env_var(&self, key: &str) -> Option<String> {
        // Try to get from actual environment first
        match env::var(key) {
            Ok(value) => Some(value),
            Err(_) => {
                // Fall back to settings
                self.env.get(key).cloned()
            }
        }
    }
}

/// Returns an environment variable with fallback to settings.
pub fn get_env_var(key: &str) -> Result<String> {
    // Try to get from actual environment first
    match env::var(key) {
        Ok(value) => Ok(value),
        Err(_) => {
            // Try to load settings and check there
            match Settings::load() {
                Ok(settings) => settings
                    .env
                    .get(key)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Environment variable not found: {}", key)),
                Err(err) => {
                    // If we couldn't load settings, just return the original env var error
                    Err(anyhow::anyhow!("Environment variable not found: {}", key).context(err))
                }
            }
        }
    }
}

/// Tries multiple environment variables with fallback to settings.
pub fn get_env_vars(keys: &[&str]) -> Result<String> {
    for key in keys {
        match get_env_var(key) {
            Ok(value) => return Ok(value),
            Err(_) => continue,
        }
    }

    Err(anyhow::anyhow!(
        "None of the environment variables found: {:?}",
        keys
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn settings_load_from_path() {
        // Create a temporary directory
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join("settings.json");

        // Create a test settings file
        let settings_json = r#"{
            "env": {
                "TEST_VAR": "test_value",
                "CLAUDE_API_KEY": "test_api_key"
            }
        }"#;
        fs::write(&settings_path, settings_json).unwrap();

        // Load settings
        let settings = Settings::load_from_path(&settings_path).unwrap();

        // Check env vars
        assert_eq!(settings.env.get("TEST_VAR").unwrap(), "test_value");
        assert_eq!(settings.env.get("CLAUDE_API_KEY").unwrap(), "test_api_key");
    }

    #[test]
    fn settings_get_env_var() {
        // Create a temporary directory
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join("settings.json");

        // Create a test settings file
        let settings_json = r#"{
            "env": {
                "TEST_VAR": "test_value",
                "CLAUDE_API_KEY": "test_api_key"
            }
        }"#;
        fs::write(&settings_path, settings_json).unwrap();

        // Load settings
        let settings = Settings::load_from_path(&settings_path).unwrap();

        // Set actual environment variable
        env::set_var("TEST_VAR_ENV", "env_value");

        // Test precedence - env var should take precedence
        env::set_var("TEST_VAR", "env_override");
        assert_eq!(settings.get_env_var("TEST_VAR").unwrap(), "env_override");

        // Test fallback to settings
        env::remove_var("TEST_VAR"); // Remove from environment
        assert_eq!(settings.get_env_var("TEST_VAR").unwrap(), "test_value");

        // Test actual env var
        assert_eq!(settings.get_env_var("TEST_VAR_ENV").unwrap(), "env_value");

        // Clean up
        env::remove_var("TEST_VAR_ENV");
    }
}
