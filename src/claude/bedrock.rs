//! Amazon Bedrock client implementation for Claude

use crate::claude::{error::ClaudeError, prompts};
use crate::config::{BedrockAuthMethod, BedrockConfig, ConfigManager};
use crate::data::{amendments::AmendmentFile, RepositoryView, RepositoryViewForAI};
use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_bedrockruntime::Client as BedrockRuntimeClient;
use serde_json::{json, Value};
use tracing::debug;

/// Bedrock client for commit message improvement using Claude on AWS Bedrock
pub struct BedrockClient {
    client: BedrockRuntimeClient,
    model_id: String,
}

impl BedrockClient {
    /// Create new Bedrock client from configuration
    pub async fn new() -> Result<Self> {
        let config_manager = ConfigManager::new();
        let settings = config_manager.load_settings()?;

        let bedrock_config = settings
            .bedrock
            .as_ref()
            .ok_or_else(|| ClaudeError::ConfigurationError("Bedrock not configured".to_string()))?;

        if !bedrock_config.enabled {
            return Err(ClaudeError::ConfigurationError(
                "Bedrock is not enabled in configuration".to_string(),
            )
            .into());
        }

        let sdk_config = Self::build_aws_config(bedrock_config).await?;
        let client = BedrockRuntimeClient::new(&sdk_config);

        let model_id = bedrock_config
            .models
            .claude
            .clone()
            .or_else(|| bedrock_config.models.default.clone())
            .unwrap_or_else(|| "anthropic.claude-3-5-sonnet-20241022-v2:0".to_string());

        Ok(Self { client, model_id })
    }

    /// Build AWS configuration based on authentication method
    async fn build_aws_config(
        config: &BedrockConfig,
    ) -> Result<aws_config::SdkConfig, ClaudeError> {
        let mut config_builder = aws_config::defaults(BehaviorVersion::latest());

        // Set region if specified
        if let Some(region) = &config.region {
            config_builder = config_builder.region(aws_config::Region::new(region.clone()));
        }

        // Configure authentication based on method
        match config.auth_method {
            BedrockAuthMethod::Environment => {
                // Use environment variables - no additional configuration needed
                debug!("Using environment-based AWS authentication");
            }
            BedrockAuthMethod::Profile => {
                if let Some(profile) = &config.profile {
                    config_builder = config_builder.profile_name(profile);
                    debug!("Using AWS profile: {}", profile);
                } else {
                    debug!("Using default AWS profile");
                }
            }
            BedrockAuthMethod::Explicit => {
                if let (Some(access_key), Some(secret_key)) = (
                    &config.auth.aws_access_key_id,
                    &config.auth.aws_secret_access_key,
                ) {
                    let credentials = if let Some(session_token) = &config.auth.aws_session_token {
                        Credentials::new(
                            access_key,
                            secret_key,
                            Some(session_token.clone()),
                            None,
                            "explicit",
                        )
                    } else {
                        Credentials::new(access_key, secret_key, None, None, "explicit")
                    };
                    config_builder = config_builder.credentials_provider(credentials);
                    debug!("Using explicit AWS credentials");
                } else {
                    return Err(ClaudeError::ConfigurationError(
                        "Explicit credentials configured but access key or secret key missing"
                            .to_string(),
                    ));
                }
            }
            BedrockAuthMethod::Sso => {
                if let Some(sso_profile) = &config.sso_profile {
                    config_builder = config_builder.profile_name(sso_profile);
                    debug!("Using AWS SSO profile: {}", sso_profile);
                } else {
                    return Err(ClaudeError::ConfigurationError(
                        "SSO authentication configured but no SSO profile specified".to_string(),
                    ));
                }
            }
            BedrockAuthMethod::IamRole => {
                // Use IAM role - no additional configuration needed
                debug!("Using IAM role authentication");
            }
        }

        Ok(config_builder.load().await)
    }

    /// Generate commit message amendments from repository view
    pub async fn generate_amendments(&self, repo_view: &RepositoryView) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate user prompt
        let user_prompt = prompts::generate_user_prompt(&repo_yaml);

        // Build the request payload for Bedrock
        let request_body = json!({
            "anthropic_version": "bedrock-2023-05-31",
            "max_tokens": 4000,
            "system": prompts::SYSTEM_PROMPT,
            "messages": [
                {
                    "role": "user",
                    "content": user_prompt
                }
            ]
        });

        // Send request to Bedrock
        let response = self
            .client
            .invoke_model()
            .model_id(&self.model_id)
            .content_type("application/json")
            .body(aws_sdk_bedrockruntime::primitives::Blob::new(
                serde_json::to_vec(&request_body).map_err(|e| {
                    ClaudeError::NetworkError(format!("Failed to serialize request: {}", e))
                })?,
            ))
            .send()
            .await
            .map_err(|e| ClaudeError::NetworkError(format!("Bedrock API error: {}", e)))?;

        // Parse response
        let response_body: Value =
            serde_json::from_slice(response.body().as_ref()).map_err(|e| {
                ClaudeError::InvalidResponseFormat(format!("Failed to parse response: {}", e))
            })?;

        // Extract content from Bedrock response
        let content = response_body
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|text| text.as_str())
            .ok_or_else(|| {
                ClaudeError::InvalidResponseFormat(
                    "No text content in Bedrock response".to_string(),
                )
            })?;

        debug!(
            content_length = content.len(),
            "Received response from Bedrock"
        );

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(content)
    }

    /// Generate contextual commit message amendments with enhanced intelligence
    pub async fn generate_contextual_amendments(
        &self,
        repo_view: &RepositoryView,
        context: &crate::data::context::CommitContext,
    ) -> Result<AmendmentFile> {
        // Convert to AI-enhanced view with diff content
        let ai_repo_view = RepositoryViewForAI::from_repository_view(repo_view.clone())
            .context("Failed to enhance repository view with diff content")?;

        // Convert repository view to YAML
        let repo_yaml = crate::data::to_yaml(&ai_repo_view)
            .context("Failed to serialize repository view to YAML")?;

        // Generate contextual prompts using Phase 3 intelligence
        let system_prompt = prompts::generate_contextual_system_prompt(context);
        let user_prompt = prompts::generate_contextual_user_prompt(&repo_yaml, context);

        // Build the request payload for Bedrock with contextual prompts
        let request_body = json!({
            "anthropic_version": "bedrock-2023-05-31",
            "max_tokens": if context.is_significant_change() { 6000 } else { 4000 },
            "system": system_prompt,
            "messages": [
                {
                    "role": "user",
                    "content": user_prompt
                }
            ]
        });

        // Send request to Bedrock
        let response = self
            .client
            .invoke_model()
            .model_id(&self.model_id)
            .content_type("application/json")
            .body(aws_sdk_bedrockruntime::primitives::Blob::new(
                serde_json::to_vec(&request_body).map_err(|e| {
                    ClaudeError::NetworkError(format!("Failed to serialize request: {}", e))
                })?,
            ))
            .send()
            .await
            .map_err(|e| ClaudeError::NetworkError(format!("Bedrock API error: {}", e)))?;

        // Parse response
        let response_body: Value =
            serde_json::from_slice(response.body().as_ref()).map_err(|e| {
                ClaudeError::InvalidResponseFormat(format!("Failed to parse response: {}", e))
            })?;

        // Extract content from Bedrock response
        let content = response_body
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|text| text.as_str())
            .ok_or_else(|| {
                ClaudeError::InvalidResponseFormat(
                    "No text content in Bedrock response".to_string(),
                )
            })?;

        debug!(
            content_length = content.len(),
            "Received contextual response from Bedrock"
        );

        // Parse YAML response to AmendmentFile
        self.parse_amendment_response(content)
    }

    /// Parse Claude's YAML response into AmendmentFile
    fn parse_amendment_response(&self, content: &str) -> Result<AmendmentFile> {
        // Extract YAML block from markdown if present
        let yaml_content = if content.contains("```yaml") {
            content
                .split("```yaml")
                .nth(1)
                .and_then(|s| s.split("```").next())
                .unwrap_or(content)
                .trim()
        } else if content.contains("```") {
            // Handle generic code blocks
            content
                .split("```")
                .nth(1)
                .and_then(|s| s.split("```").next())
                .unwrap_or(content)
                .trim()
        } else {
            content.trim()
        };

        // Try to parse YAML
        let amendment_file: AmendmentFile = serde_yaml::from_str(yaml_content).map_err(|e| {
            debug!(
                error = %e,
                content_length = content.len(),
                yaml_length = yaml_content.len(),
                "YAML parsing failed"
            );
            debug!(content = %content, "Raw Bedrock response");
            debug!(yaml = %yaml_content, "Extracted YAML content");

            // Try to provide more helpful error messages for common issues
            if yaml_content.lines().any(|line| line.contains('\t')) {
                ClaudeError::AmendmentParsingFailed("YAML parsing error: Found tab characters. YAML requires spaces for indentation.".to_string())
            } else if yaml_content.lines().any(|line| line.trim().starts_with('-') && !line.trim().starts_with("- ")) {
                ClaudeError::AmendmentParsingFailed("YAML parsing error: List items must have a space after the dash (- item).".to_string())
            } else {
                ClaudeError::AmendmentParsingFailed(format!("YAML parsing error: {}", e))
            }
        })?;

        // Validate the parsed amendments
        amendment_file
            .validate()
            .map_err(|e| ClaudeError::AmendmentParsingFailed(format!("Validation error: {}", e)))?;

        Ok(amendment_file)
    }

    /// Check if Bedrock is available and configured
    pub async fn is_available() -> bool {
        ConfigManager::new()
            .is_bedrock_enabled()
            .unwrap_or_default()
    }

    /// Get the model ID being used
    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BedrockConfig, BedrockModels, ClaudeSettings};
    use tempfile::tempdir;

    #[test]
    fn test_bedrock_config_validation() {
        let config = BedrockConfig {
            enabled: true,
            region: Some("us-east-1".to_string()),
            auth_method: BedrockAuthMethod::Profile,
            profile: Some("test-profile".to_string()),
            models: BedrockModels::default(),
            ..Default::default()
        };

        assert_eq!(config.enabled, true);
        assert_eq!(config.auth_method, BedrockAuthMethod::Profile);
        assert_eq!(config.profile, Some("test-profile".to_string()));
    }

    #[tokio::test]
    async fn test_bedrock_client_creation_fails_without_config() {
        // This should fail because there's no configuration
        let result = BedrockClient::new().await;
        assert!(result.is_err());
    }

    #[test]
    fn test_bedrock_availability_check() {
        let temp_dir = tempdir().unwrap();
        let settings_path = temp_dir.path().join("settings.json");
        let manager = ConfigManager::with_path(settings_path);

        // Should be false by default
        let result = manager.is_bedrock_enabled().unwrap();
        assert!(!result);

        // Should be true when enabled
        let settings = ClaudeSettings {
            bedrock: Some(BedrockConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        manager.save_settings(&settings).unwrap();

        let result = manager.is_bedrock_enabled().unwrap();
        assert!(result);
    }
}
