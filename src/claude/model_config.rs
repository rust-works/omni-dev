//! AI model configuration and specifications
//!
//! This module provides model specifications loaded from embedded YAML templates
//! to ensure correct API parameters for different AI models.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Model specification from YAML configuration
#[derive(Debug, Deserialize, Clone)]
pub struct ModelSpec {
    /// AI provider name (e.g., "claude")
    pub provider: String,
    /// Human-readable model name (e.g., "Claude Opus 4")
    pub model: String,
    /// API identifier used for requests (e.g., "claude-3-opus-20240229")
    pub api_identifier: String,
    /// Maximum number of tokens that can be generated in a single response
    pub max_output_tokens: usize,
    /// Maximum number of tokens that can be included in the input context
    pub input_context: usize,
    /// Model generation number (e.g., 3.0, 3.5, 4.0)
    pub generation: f32,
    /// Performance tier (e.g., "fast", "balanced", "flagship")
    pub tier: String,
    /// Whether this is a legacy model that may be deprecated
    #[serde(default)]
    pub legacy: bool,
}

/// Model tier information
#[derive(Debug, Deserialize)]
pub struct TierInfo {
    /// Human-readable description of the tier
    pub description: String,
    /// List of recommended use cases for this tier
    pub use_cases: Vec<String>,
}

/// Default fallback configuration for a provider
#[derive(Debug, Deserialize)]
pub struct DefaultConfig {
    /// Default maximum output tokens for unknown models from this provider
    pub max_output_tokens: usize,
    /// Default input context limit for unknown models from this provider
    pub input_context: usize,
}

/// Provider-specific configuration
#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    /// Human-readable provider name
    pub name: String,
    /// Base URL for API requests
    pub api_base: String,
    /// Default model identifier to use if none specified
    pub default_model: String,
    /// Available performance tiers and their descriptions
    pub tiers: HashMap<String, TierInfo>,
    /// Default configuration for unknown models
    pub defaults: DefaultConfig,
}

/// Complete model configuration
#[derive(Debug, Deserialize)]
pub struct ModelConfiguration {
    /// List of all available models
    pub models: Vec<ModelSpec>,
    /// Provider-specific configurations
    pub providers: HashMap<String, ProviderConfig>,
}

/// Model registry for looking up specifications
pub struct ModelRegistry {
    config: ModelConfiguration,
    by_identifier: HashMap<String, ModelSpec>,
    by_provider: HashMap<String, Vec<ModelSpec>>,
}

impl ModelRegistry {
    /// Load model registry from embedded YAML
    pub fn load() -> Result<Self> {
        let yaml_content = include_str!("../templates/models.yaml");
        let config: ModelConfiguration = serde_yaml::from_str(yaml_content)?;

        // Build lookup maps
        let mut by_identifier = HashMap::new();
        let mut by_provider: HashMap<String, Vec<ModelSpec>> = HashMap::new();

        for model in &config.models {
            by_identifier.insert(model.api_identifier.clone(), model.clone());
            by_provider
                .entry(model.provider.clone())
                .or_default()
                .push(model.clone());
        }

        Ok(Self {
            config,
            by_identifier,
            by_provider,
        })
    }

    /// Get model specification by API identifier
    pub fn get_model_spec(&self, api_identifier: &str) -> Option<&ModelSpec> {
        // Try exact match first
        if let Some(spec) = self.by_identifier.get(api_identifier) {
            return Some(spec);
        }

        // Try fuzzy matching for Bedrock-style identifiers
        self.find_model_by_fuzzy_match(api_identifier)
    }

    /// Get max output tokens for a model, with fallback to provider defaults
    pub fn get_max_output_tokens(&self, api_identifier: &str) -> usize {
        if let Some(spec) = self.get_model_spec(api_identifier) {
            return spec.max_output_tokens;
        }

        // Try to infer provider from model identifier and use defaults
        if let Some(provider) = self.infer_provider(api_identifier) {
            if let Some(provider_config) = self.config.providers.get(&provider) {
                return provider_config.defaults.max_output_tokens;
            }
        }

        // Ultimate fallback
        4096
    }

    /// Get input context limit for a model, with fallback to provider defaults
    pub fn get_input_context(&self, api_identifier: &str) -> usize {
        if let Some(spec) = self.get_model_spec(api_identifier) {
            return spec.input_context;
        }

        // Try to infer provider from model identifier and use defaults
        if let Some(provider) = self.infer_provider(api_identifier) {
            if let Some(provider_config) = self.config.providers.get(&provider) {
                return provider_config.defaults.input_context;
            }
        }

        // Ultimate fallback
        100000
    }

    /// Infer provider from model identifier
    fn infer_provider(&self, api_identifier: &str) -> Option<String> {
        if api_identifier.starts_with("claude") || api_identifier.contains("anthropic") {
            Some("claude".to_string())
        } else {
            None
        }
    }

    /// Find model by fuzzy matching for various identifier formats
    fn find_model_by_fuzzy_match(&self, api_identifier: &str) -> Option<&ModelSpec> {
        // Extract core model identifier from various formats:
        // - Bedrock: "us.anthropic.claude-3-7-sonnet-20250219-v1:0" -> "claude-3-7-sonnet-20250219"
        // - AWS: "anthropic.claude-3-haiku-20240307-v1:0" -> "claude-3-haiku-20240307"
        // - Standard: "claude-3-opus-20240229" -> "claude-3-opus-20240229"

        let core_identifier = self.extract_core_model_identifier(api_identifier);

        // Try to find exact match with core identifier
        if let Some(spec) = self.by_identifier.get(&core_identifier) {
            return Some(spec);
        }

        // Try partial matching - look for models that contain the core parts
        for (stored_id, spec) in &self.by_identifier {
            if self.models_match_fuzzy(&core_identifier, stored_id) {
                return Some(spec);
            }
        }

        None
    }

    /// Extract core model identifier from various formats
    fn extract_core_model_identifier(&self, api_identifier: &str) -> String {
        let mut identifier = api_identifier.to_string();

        // Remove region prefixes (us., eu., etc.)
        if let Some(dot_pos) = identifier.find('.') {
            if identifier[..dot_pos].len() <= 3 {
                // likely a region code
                identifier = identifier[dot_pos + 1..].to_string();
            }
        }

        // Remove provider prefixes (anthropic.)
        if identifier.starts_with("anthropic.") {
            identifier = identifier["anthropic.".len()..].to_string();
        }

        // Remove version suffixes (-v1:0, -v2:1, etc.)
        if let Some(version_pos) = identifier.rfind("-v") {
            if identifier[version_pos..].contains(':') {
                identifier = identifier[..version_pos].to_string();
            }
        }

        identifier
    }

    /// Check if two model identifiers represent the same model
    fn models_match_fuzzy(&self, input_id: &str, stored_id: &str) -> bool {
        // For now, just check if they're the same after extraction
        // This could be enhanced with more sophisticated matching
        input_id == stored_id
    }

    /// Check if a model is legacy
    pub fn is_legacy_model(&self, api_identifier: &str) -> bool {
        self.get_model_spec(api_identifier)
            .map(|spec| spec.legacy)
            .unwrap_or(false)
    }

    /// Get all available models
    pub fn get_all_models(&self) -> &[ModelSpec] {
        &self.config.models
    }

    /// Get models by provider
    pub fn get_models_by_provider(&self, provider: &str) -> Vec<&ModelSpec> {
        self.by_provider
            .get(provider)
            .map(|models| models.iter().collect())
            .unwrap_or_default()
    }

    /// Get models by provider and tier
    pub fn get_models_by_provider_and_tier(&self, provider: &str, tier: &str) -> Vec<&ModelSpec> {
        self.get_models_by_provider(provider)
            .into_iter()
            .filter(|model| model.tier == tier)
            .collect()
    }

    /// Get provider configuration
    pub fn get_provider_config(&self, provider: &str) -> Option<&ProviderConfig> {
        self.config.providers.get(provider)
    }

    /// Get tier information for a provider
    pub fn get_tier_info(&self, provider: &str, tier: &str) -> Option<&TierInfo> {
        self.config.providers.get(provider)?.tiers.get(tier)
    }
}

/// Global model registry instance
static MODEL_REGISTRY: OnceLock<ModelRegistry> = OnceLock::new();

/// Get the global model registry instance
pub fn get_model_registry() -> &'static ModelRegistry {
    MODEL_REGISTRY.get_or_init(|| ModelRegistry::load().expect("Failed to load model registry"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_model_registry() {
        let registry = ModelRegistry::load().unwrap();
        assert!(!registry.config.models.is_empty());
        assert!(registry.config.providers.contains_key("claude"));
    }

    #[test]
    fn test_claude_model_lookup() {
        let registry = ModelRegistry::load().unwrap();

        // Test legacy Claude 3 Opus
        let opus_spec = registry.get_model_spec("claude-3-opus-20240229");
        assert!(opus_spec.is_some());
        assert_eq!(opus_spec.unwrap().max_output_tokens, 4096);
        assert_eq!(opus_spec.unwrap().provider, "claude");
        assert!(registry.is_legacy_model("claude-3-opus-20240229"));

        // Test newer model
        let sonnet4_tokens = registry.get_max_output_tokens("claude-sonnet-4-20250514");
        assert_eq!(sonnet4_tokens, 64000);

        // Test unknown model falls back to provider defaults
        let unknown_tokens = registry.get_max_output_tokens("claude-unknown-model");
        assert_eq!(unknown_tokens, 4096); // Should use Claude provider defaults
    }

    #[test]
    fn test_provider_filtering() {
        let registry = ModelRegistry::load().unwrap();

        let claude_models = registry.get_models_by_provider("claude");
        assert!(!claude_models.is_empty());

        let fast_claude_models = registry.get_models_by_provider_and_tier("claude", "fast");
        assert!(!fast_claude_models.is_empty());

        let tier_info = registry.get_tier_info("claude", "fast");
        assert!(tier_info.is_some());
    }

    #[test]
    fn test_provider_config() {
        let registry = ModelRegistry::load().unwrap();

        let claude_config = registry.get_provider_config("claude");
        assert!(claude_config.is_some());
        assert_eq!(claude_config.unwrap().name, "Anthropic Claude");
    }

    #[test]
    fn test_fuzzy_model_matching() {
        let registry = ModelRegistry::load().unwrap();

        // Test Bedrock-style identifiers
        let bedrock_3_7_sonnet = "us.anthropic.claude-3-7-sonnet-20250219-v1:0";
        let spec = registry.get_model_spec(bedrock_3_7_sonnet);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().api_identifier, "claude-3-7-sonnet-20250219");
        assert_eq!(spec.unwrap().max_output_tokens, 64000);

        // Test AWS-style identifiers
        let aws_haiku = "anthropic.claude-3-haiku-20240307-v1:0";
        let spec = registry.get_model_spec(aws_haiku);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().api_identifier, "claude-3-haiku-20240307");
        assert_eq!(spec.unwrap().max_output_tokens, 4096);

        // Test European region
        let eu_opus = "eu.anthropic.claude-3-opus-20240229-v2:1";
        let spec = registry.get_model_spec(eu_opus);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().api_identifier, "claude-3-opus-20240229");
        assert_eq!(spec.unwrap().max_output_tokens, 4096);

        // Test exact match still works
        let exact_sonnet4 = "claude-sonnet-4-20250514";
        let spec = registry.get_model_spec(exact_sonnet4);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().max_output_tokens, 64000);
    }

    #[test]
    fn test_extract_core_model_identifier() {
        let registry = ModelRegistry::load().unwrap();

        // Test various formats
        assert_eq!(
            registry.extract_core_model_identifier("us.anthropic.claude-3-7-sonnet-20250219-v1:0"),
            "claude-3-7-sonnet-20250219"
        );

        assert_eq!(
            registry.extract_core_model_identifier("anthropic.claude-3-haiku-20240307-v1:0"),
            "claude-3-haiku-20240307"
        );

        assert_eq!(
            registry.extract_core_model_identifier("claude-3-opus-20240229"),
            "claude-3-opus-20240229"
        );

        assert_eq!(
            registry.extract_core_model_identifier("eu.anthropic.claude-sonnet-4-20250514-v2:1"),
            "claude-sonnet-4-20250514"
        );
    }
}
