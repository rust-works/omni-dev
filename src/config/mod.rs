//! Configuration management for Claude Code settings integration

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Claude Code settings configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClaudeSettings {
    /// Anthropic API configuration
    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,

    /// Amazon Bedrock configuration
    #[serde(default)]
    pub bedrock: Option<BedrockConfig>,

    /// API key helper configuration
    #[serde(default, rename = "apiKeyHelper")]
    pub api_key_helper: Option<ApiKeyHelperConfig>,

    /// Default AI provider to use
    #[serde(default, rename = "defaultProvider")]
    pub default_provider: Option<String>,
}

/// Anthropic API configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicConfig {
    /// API key (stored in keychain, this is just for reference)
    #[serde(default, rename = "apiKey")]
    pub api_key: Option<String>,

    /// Default model to use
    #[serde(default, rename = "defaultModel")]
    pub default_model: Option<String>,
}

/// Amazon Bedrock configuration
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BedrockConfig {
    /// Whether Bedrock integration is enabled
    #[serde(default)]
    pub enabled: bool,

    /// AWS region to use
    #[serde(default)]
    pub region: Option<String>,

    /// AWS authentication method
    #[serde(default, rename = "authMethod")]
    pub auth_method: BedrockAuthMethod,

    /// AWS profile to use
    #[serde(default)]
    pub profile: Option<String>,

    /// SSO profile for AWS SSO authentication
    #[serde(default, rename = "ssoProfile")]
    pub sso_profile: Option<String>,

    /// Model configuration
    #[serde(default)]
    pub models: BedrockModels,

    /// Authentication configuration
    #[serde(default)]
    pub auth: BedrockAuth,
}

/// Bedrock authentication methods
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum BedrockAuthMethod {
    /// Use AWS CLI profile
    #[default]
    Profile,
    /// Use environment variables
    Environment,
    /// Use IAM role (for EC2/Lambda)
    IamRole,
    /// Use AWS SSO
    Sso,
    /// Use explicit credentials (not recommended)
    Explicit,
}

/// Bedrock model configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BedrockModels {
    /// Claude model ID
    #[serde(default)]
    pub claude: Option<String>,

    /// Default model to use
    #[serde(default)]
    pub default: Option<String>,

    /// Additional models
    #[serde(flatten)]
    pub additional: HashMap<String, String>,
}

impl Default for BedrockModels {
    fn default() -> Self {
        Self {
            claude: Some("anthropic.claude-3-5-sonnet-20241022-v2:0".to_string()),
            default: Some("anthropic.claude-3-5-sonnet-20241022-v2:0".to_string()),
            additional: HashMap::new(),
        }
    }
}

/// Bedrock authentication configuration
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BedrockAuth {
    /// AWS access key ID
    #[serde(default, rename = "awsAccessKeyId")]
    pub aws_access_key_id: Option<String>,

    /// AWS secret access key
    #[serde(default, rename = "awsSecretAccessKey")]
    pub aws_secret_access_key: Option<String>,

    /// AWS session token
    #[serde(default, rename = "awsSessionToken")]
    pub aws_session_token: Option<String>,

    /// Command to refresh credentials
    #[serde(default, rename = "refreshCommand")]
    pub refresh_command: Option<String>,

    /// Credential export configuration
    #[serde(default, rename = "credentialExport")]
    pub credential_export: CredentialExportConfig,
}

/// Credential export configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CredentialExportConfig {
    /// Whether credential export is enabled
    #[serde(default)]
    pub enabled: bool,

    /// Command to export credentials
    #[serde(default)]
    pub command: Option<String>,
}

impl Default for CredentialExportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: Some("aws configure export-credentials --format env".to_string()),
        }
    }
}

/// API key helper configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiKeyHelperConfig {
    /// Command to run to get API key
    #[serde(default)]
    pub command: Option<String>,

    /// TTL in milliseconds
    #[serde(default, rename = "ttlMs")]
    pub ttl_ms: Option<u64>,

    /// Whether the helper is enabled
    #[serde(default)]
    pub enabled: bool,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            anthropic: Some(AnthropicConfig {
                api_key: None,
                default_model: Some("claude-3-5-sonnet-20241022".to_string()),
            }),
            bedrock: None,
            api_key_helper: None,
            default_provider: Some("anthropic".to_string()),
        }
    }
}

/// Configuration manager for Claude Code settings
pub struct ConfigManager {
    settings_path: PathBuf,
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self {
            settings_path: Self::default_settings_path(),
        }
    }
}

impl ConfigManager {
    /// Create a new configuration manager
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a configuration manager with custom settings path
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            settings_path: path,
        }
    }

    /// Get the default settings path (~/.claude/settings.json)
    pub fn default_settings_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude")
            .join("settings.json")
    }

    /// Load settings from file
    pub fn load_settings(&self) -> Result<ClaudeSettings> {
        if !self.settings_path.exists() {
            return Ok(ClaudeSettings::default());
        }

        let content = std::fs::read_to_string(&self.settings_path)
            .with_context(|| format!("Failed to read settings file: {:?}", self.settings_path))?;

        let settings: ClaudeSettings = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse settings file: {:?}", self.settings_path))?;

        Ok(settings)
    }

    /// Save settings to file
    pub fn save_settings(&self, settings: &ClaudeSettings) -> Result<()> {
        // Ensure directory exists
        if let Some(parent) = self.settings_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create settings directory: {:?}", parent))?;
        }

        let content =
            serde_json::to_string_pretty(settings).context("Failed to serialize settings")?;

        std::fs::write(&self.settings_path, content)
            .with_context(|| format!("Failed to write settings file: {:?}", self.settings_path))?;

        Ok(())
    }

    /// Check if Bedrock is enabled and configured
    pub fn is_bedrock_enabled(&self) -> Result<bool> {
        let settings = self.load_settings()?;
        Ok(settings
            .bedrock
            .as_ref()
            .map(|b| b.enabled)
            .unwrap_or(false))
    }

    /// Get the active provider (anthropic or bedrock)
    pub fn get_active_provider(&self) -> Result<String> {
        let settings = self.load_settings()?;

        // Check if bedrock is enabled and configured
        if let Some(bedrock) = &settings.bedrock {
            if bedrock.enabled {
                return Ok("bedrock".to_string());
            }
        }

        // Fall back to default provider or anthropic
        Ok(settings
            .default_provider
            .unwrap_or_else(|| "anthropic".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_default_settings() {
        let settings = ClaudeSettings::default();
        assert!(settings.anthropic.is_some());
        assert_eq!(settings.default_provider, Some("anthropic".to_string()));
    }

    #[test]
    fn test_bedrock_config_serialization() {
        let bedrock = BedrockConfig {
            enabled: true,
            region: Some("us-east-1".to_string()),
            auth_method: BedrockAuthMethod::Profile,
            profile: Some("default".to_string()),
            sso_profile: None,
            models: BedrockModels::default(),
            auth: BedrockAuth::default(),
        };

        let json = serde_json::to_string(&bedrock).unwrap();
        let deserialized: BedrockConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.enabled, true);
        assert_eq!(deserialized.region, Some("us-east-1".to_string()));
    }

    #[test]
    fn test_config_manager_load_save() {
        let temp_dir = tempdir().unwrap();
        let settings_path = temp_dir.path().join("settings.json");
        let manager = ConfigManager::with_path(settings_path.clone());

        // Test loading non-existent file returns defaults
        let settings = manager.load_settings().unwrap();
        assert!(settings.anthropic.is_some());

        // Test saving and loading
        let mut settings = ClaudeSettings::default();
        settings.bedrock = Some(BedrockConfig {
            enabled: true,
            region: Some("us-west-2".to_string()),
            ..Default::default()
        });

        manager.save_settings(&settings).unwrap();
        assert!(settings_path.exists());

        let loaded = manager.load_settings().unwrap();
        assert!(loaded.bedrock.is_some());
        assert_eq!(
            loaded.bedrock.unwrap().region,
            Some("us-west-2".to_string())
        );
    }

    #[test]
    fn test_is_bedrock_enabled() {
        let temp_dir = tempdir().unwrap();
        let settings_path = temp_dir.path().join("settings.json");
        let manager = ConfigManager::with_path(settings_path);

        // Default should be false
        assert!(!manager.is_bedrock_enabled().unwrap());

        // Save settings with bedrock enabled
        let settings = ClaudeSettings {
            bedrock: Some(BedrockConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        manager.save_settings(&settings).unwrap();
        assert!(manager.is_bedrock_enabled().unwrap());
    }
}
