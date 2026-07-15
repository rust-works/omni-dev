//! AI model configuration and specifications.
//!
//! # Data model
//!
//! [`ModelConfiguration`] is the top-level container. It owns a
//! <code>Vec<[ModelSpec]></code> of every known model and a
//! <code>HashMap<String, [ProviderConfig]></code> keyed by provider name.
//! [`ModelSpec`] records the per-model limits, generation, tier name, and
//! any [`BetaHeader`]s that unlock enhanced limits. [`ProviderConfig`]
//! records provider-wide settings — including a [`TierInfo`] map describing
//! each named tier and a [`DefaultConfig`] block used as the fallback for
//! unknown identifiers from that provider. Every entry carries a
//! [`ModelSource`] tag identifying which layer contributed it.
//!
//! [`ModelRegistry`] wraps a fully merged [`ModelConfiguration`] and adds
//! identifier-normalised lookup (so a Bedrock or AWS-direct identifier
//! resolves to the same [`ModelSpec`] as the canonical Anthropic form).
//!
//! # Loader
//!
//! [`ModelRegistry::load`] builds the registry from a layered set of YAML
//! sources: an embedded catalog (compile-time `include_str!`), an optional
//! user-level file at `~/.omni-dev/models.yaml`, and an optional
//! project-local file at `./.omni-dev/models.yaml`. Layers are deep-merged
//! with project > user > embedded precedence; an explicit override path
//! provided via `OMNI_DEV_MODELS_YAML` short-circuits the user/project
//! lookup. See [ADR-0022](../../docs/adrs/adr-0022.md) for the layered
//! loader rationale and [ADR-0011](../../docs/adrs/adr-0011.md) for the
//! original compile-time design.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Embedded models YAML configuration, loaded at compile time.
pub(crate) const MODELS_YAML: &str = include_str!("../templates/models.yaml");

/// Schema version that this build of omni-dev understands.
///
/// User/project files declaring a different version receive a warning at
/// load time. Files without a `version:` field are accepted with a warning
/// for backwards compatibility.
pub const MODELS_SCHEMA_VERSION: &str = "1";

/// Environment variable that, when set, points at a single user-side YAML
/// file and short-circuits the standard user/project lookup.
pub const OMNI_DEV_MODELS_YAML_ENV: &str = "OMNI_DEV_MODELS_YAML";

/// Ultimate fallback max output tokens when no model or provider config matches.
const FALLBACK_MAX_OUTPUT_TOKENS: usize = 4096;

/// Ultimate fallback input context when no model or provider config matches.
const FALLBACK_INPUT_CONTEXT: usize = 100_000;

/// Layer that contributed a model or provider entry.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    /// Compile-time embedded catalog (`src/templates/models.yaml`).
    #[default]
    Embedded,
    /// User-level catalog at `~/.omni-dev/models.yaml`.
    User,
    /// Project-local catalog at `./.omni-dev/models.yaml`.
    Project,
    /// File explicitly pointed to by `OMNI_DEV_MODELS_YAML`/`--models-yaml`.
    Override,
}

impl std::fmt::Display for ModelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Embedded => "embedded",
            Self::User => "user",
            Self::Project => "project",
            Self::Override => "override",
        })
    }
}

/// HTTP header that, when sent on a request, unlocks enhanced limits for a
/// model.
///
/// A [`BetaHeader`] is a leaf of a [`ModelSpec`]: it names the header to
/// send (`key`/`value`) and records the new ceiling for [`max_output_tokens`]
/// and/or [`input_context`] that the header makes available. An absent
/// override field means that header does not move that limit; the model's
/// base value still applies. Callers consult these via
/// [`ModelRegistry::get_max_output_tokens_with_beta`] and
/// [`ModelRegistry::get_input_context_with_beta`].
///
/// [`max_output_tokens`]: ModelSpec::max_output_tokens
/// [`input_context`]: ModelSpec::input_context
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BetaHeader {
    /// HTTP header name (e.g., "anthropic-beta").
    pub key: String,
    /// Header value (e.g., "context-1m-2025-08-07").
    pub value: String,
    /// Overridden max output tokens when this header is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    /// Overridden input context when this header is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_context: Option<usize>,
}

/// Specification for a single model: its identity, limits, tier, and any
/// beta-header unlocks.
///
/// A [`ModelSpec`] is the central row of the registry. `provider` and
/// `tier` cross-reference into a [`ProviderConfig`] (via
/// [`ModelConfiguration::providers`] and [`ProviderConfig::tiers`]).
/// `max_output_tokens` and `input_context` are the *base* limits; entries
/// in `beta_headers` raise them when the corresponding HTTP header is sent.
/// `source` is loader-populated and records which layer contributed the
/// entry — never read from YAML.
///
/// # Identifier normalization
///
/// The same underlying model is addressable through several identifier
/// formats depending on how the API is reached:
///
/// - Canonical (Anthropic direct): `claude-3-7-sonnet-20250219`
/// - Bedrock with region prefix: `us.anthropic.claude-3-7-sonnet-20250219-v1:0`
/// - AWS-direct without region: `anthropic.claude-3-haiku-20240307-v1:0`
/// - Regional gateways: `eu.anthropic.claude-3-opus-20240229-v2:1`
///
/// All four resolve to the same [`ModelSpec`]:
/// [`ModelRegistry::get_model_spec`] tries an exact match first, and on
/// miss strips region/provider prefixes and version suffixes before
/// retrying. See [ADR-0011](../../docs/adrs/adr-0011.md) for the design
/// rationale.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ModelSpec {
    /// AI provider name (e.g., "claude").
    pub provider: String,
    /// Human-readable model name (e.g., "Claude Opus 4").
    pub model: String,
    /// API identifier used for requests (e.g., "claude-3-opus-20240229").
    pub api_identifier: String,
    /// Maximum number of tokens that can be generated in a single response.
    pub max_output_tokens: usize,
    /// Maximum number of tokens that can be included in the input context.
    pub input_context: usize,
    /// Model generation number (e.g., 3.0, 3.5, 4.0).
    pub generation: f32,
    /// Performance tier (e.g., "fast", "balanced", "flagship").
    pub tier: String,
    /// Whether this is a legacy model that may be deprecated.
    #[serde(default)]
    pub legacy: bool,
    /// Announced retirement date (`YYYY-MM-DD`), if the provider has published
    /// one.
    ///
    /// Complements [`Self::legacy`], which cannot distinguish "older but fine"
    /// from "stops working on a known date" (#1334). `None` covers both models
    /// with no announced retirement and deprecated models whose date is still
    /// TBD — absence means "no published date", never "not retiring".
    ///
    /// A past date means the model is already retired and its identifier now
    /// `404`s; such entries are retained only so Bedrock/AWS identifier
    /// normalization keeps resolving them (see ADR-0011).
    ///
    /// Held as a plain `String`: nothing parses or compares it today, it is
    /// carried verbatim into `omni-dev config models show`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retires: Option<String>,
    /// Whether this model supports structured JSON-schema output via the
    /// Anthropic Messages API `output_config.format` (and the equivalent on
    /// Bedrock).
    ///
    /// Gates the direct-API / Bedrock schema path (#1119): only models
    /// flagged here advertise
    /// [`AiClientCapabilities::supports_response_schema`](crate::claude::ai::AiClientCapabilities::supports_response_schema),
    /// so a caller on an older model — which would `400` on
    /// `output_config` — transparently keeps the YAML fallback. Defaults to
    /// `false`, so unmarked and unknown models are never sent the field.
    #[serde(default)]
    pub supports_structured_output: bool,
    /// Price per million *input* tokens in USD, if known.
    ///
    /// Used to compute per-invocation cost for backends that report token
    /// usage (currently the direct Anthropic API — see
    /// [`crate::claude::ai::compute_cost_usd`]). `None` for unpriced models
    /// (e.g. the OpenAI/Gemini entries), which surface cost as unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_token_price: Option<f64>,
    /// Price per million *output* tokens in USD, if known. See
    /// [`Self::input_token_price`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_price: Option<f64>,
    /// Beta headers that unlock enhanced limits for this model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub beta_headers: Vec<BetaHeader>,
    /// Layer that contributed this entry. Populated by the loader; never
    /// read from YAML.
    #[serde(default, skip_deserializing)]
    pub source: ModelSource,
}

/// Human-readable metadata for a named performance tier.
///
/// A tier groups models with comparable speed/capability trade-offs
/// (e.g. `fast`, `balanced`, `flagship`). [`TierInfo`] holds only the
/// *description* and recommended use cases — the *limits* (output tokens,
/// input context, beta-header unlocks) live on each [`ModelSpec`], not
/// here. [`TierInfo`] is stored in [`ProviderConfig::tiers`] keyed by tier
/// name, and the same tier name appears on [`ModelSpec::tier`] to link a
/// model into its tier.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TierInfo {
    /// Human-readable description of the tier.
    pub description: String,
    /// List of recommended use cases for this tier.
    pub use_cases: Vec<String>,
}

/// Provider-wide fallback limits used when a requested identifier does not
/// match any [`ModelSpec`].
///
/// [`ModelRegistry::get_max_output_tokens`] and
/// [`ModelRegistry::get_input_context`] consult these values whenever the
/// caller passes an identifier the registry has not seen — typically a
/// brand-new model the embedded catalog has not yet been updated for, but
/// whose provider can still be inferred from the identifier shape. If the
/// provider itself cannot be inferred, an ultimate hard-coded fallback in
/// this module applies instead.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DefaultConfig {
    /// Default maximum output tokens for unknown models from this provider.
    pub max_output_tokens: usize,
    /// Default input context limit for unknown models from this provider.
    pub input_context: usize,
}

/// Per-provider settings: endpoint, default model, named tiers, and the
/// fallback limits for unknown identifiers.
///
/// One [`ProviderConfig`] exists per AI vendor (Anthropic Claude, OpenAI,
/// Bedrock, Ollama, …) and is stored in [`ModelConfiguration::providers`]
/// keyed by provider name. `tiers` maps tier names to [`TierInfo`]
/// descriptions; the same names appear on [`ModelSpec::tier`]. `defaults`
/// is the per-provider [`DefaultConfig`] used as a fallback when a model
/// identifier does not match any [`ModelSpec`]. `source` is
/// loader-populated and records the highest-precedence layer that
/// contributed any field to this provider block.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProviderConfig {
    /// Human-readable provider name.
    pub name: String,
    /// Base URL for API requests.
    pub api_base: String,
    /// Default model identifier to use if none specified.
    pub default_model: String,
    /// Available performance tiers and their descriptions.
    pub tiers: HashMap<String, TierInfo>,
    /// Default configuration for unknown models.
    pub defaults: DefaultConfig,
    /// Layer that contributed this provider block. Populated by the loader.
    #[serde(default, skip_deserializing)]
    pub source: ModelSource,
}

/// Top-level deserialised model catalog: every known model plus every
/// provider's settings.
///
/// [`ModelConfiguration`] is the result of merging the embedded
/// `src/templates/models.yaml` with any optional user
/// (`~/.omni-dev/models.yaml`) and project (`./.omni-dev/models.yaml`)
/// overrides, in that precedence order. See
/// [ADR-0022](../../docs/adrs/adr-0022.md) for the layered loader and
/// merge semantics. The canonical entry point that produces a fully merged
/// instance — and wraps it in lookup indices — is [`ModelRegistry::load`];
/// the raw configuration is reachable from there via
/// [`ModelRegistry::config`].
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ModelConfiguration {
    /// Schema version declared by the source YAML, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// List of all available models.
    pub models: Vec<ModelSpec>,
    /// Provider-specific configurations.
    pub providers: HashMap<String, ProviderConfig>,
}

/// Indexed view over a [`ModelConfiguration`] with identifier-normalised
/// lookup.
///
/// [`ModelRegistry`] owns the merged catalog and two auxiliary indices —
/// by API identifier and by provider — populated at construction time.
/// Construct one with [`ModelRegistry::load`], which performs the layered
/// YAML load described on [`ModelConfiguration`]. Most callers use the
/// process-wide singleton returned by [`get_model_registry`] rather than
/// loading their own instance.
pub struct ModelRegistry {
    config: ModelConfiguration,
    by_identifier: HashMap<String, ModelSpec>,
    by_provider: HashMap<String, Vec<ModelSpec>>,
}

impl ModelRegistry {
    /// Loads the model registry, layering an optional user-side catalog
    /// over the embedded one.
    ///
    /// Lookup order (highest precedence wins):
    /// 1. `OMNI_DEV_MODELS_YAML` — explicit override path; short-circuits 2 & 3.
    /// 2. `./.omni-dev/models.yaml` — project-local catalog (if present).
    /// 3. `~/.omni-dev/models.yaml` — user-level catalog (if present).
    /// 4. Embedded `src/templates/models.yaml` — always present, lowest layer.
    ///
    /// Missing user-side files fall through silently. Malformed user-side
    /// files log an error and are skipped. A malformed embedded catalog is
    /// a hard failure (compile-time invariant).
    pub fn load() -> Result<Self> {
        let override_path = std::env::var(OMNI_DEV_MODELS_YAML_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let project_path = default_project_path();
        let user_path = default_user_path();
        Self::load_layered_from_paths(
            project_path.as_deref(),
            user_path.as_deref(),
            override_path.as_deref(),
        )
    }

    /// Loads the registry with explicit paths for the user-side layers.
    ///
    /// Exposed primarily for testing — the public entry point is `load()`.
    pub fn load_layered_from_paths(
        project_path: Option<&Path>,
        user_path: Option<&Path>,
        override_path: Option<&Path>,
    ) -> Result<Self> {
        let mut layers: Vec<(ModelSource, String)> = Vec::new();
        layers.push((ModelSource::Embedded, MODELS_YAML.to_string()));

        if let Some(path) = override_path {
            match read_optional_yaml(path) {
                Some(yaml) => layers.push((ModelSource::Override, yaml)),
                None => {
                    tracing::warn!(
                        "{OMNI_DEV_MODELS_YAML_ENV} points at {} but the file is missing or unreadable; falling back to embedded catalog",
                        path.display()
                    );
                }
            }
        } else {
            if let Some(path) = user_path {
                if let Some(yaml) = read_optional_yaml(path) {
                    layers.push((ModelSource::User, yaml));
                }
            }
            if let Some(path) = project_path {
                if let Some(yaml) = read_optional_yaml(path) {
                    layers.push((ModelSource::Project, yaml));
                }
            }
        }

        Self::from_layers(&layers)
    }

    /// Builds the registry from already-loaded YAML sources.
    ///
    /// `layers` must be ordered from lowest to highest precedence; the
    /// first entry is treated as the embedded catalog and a parse failure
    /// there is a hard error.
    pub(crate) fn from_layers(layers: &[(ModelSource, String)]) -> Result<Self> {
        let mut merged: serde_yaml::Value =
            serde_yaml::Value::Mapping(serde_yaml::Mapping::default());
        let mut model_sources: HashMap<String, ModelSource> = HashMap::new();
        let mut provider_sources: HashMap<String, ModelSource> = HashMap::new();
        let mut declared_versions: Vec<(ModelSource, Option<String>)> = Vec::new();

        for (source, yaml) in layers {
            let value: serde_yaml::Value = match serde_yaml::from_str(yaml) {
                Ok(v) => v,
                Err(e) => {
                    if matches!(source, ModelSource::Embedded) {
                        return Err(anyhow!(
                            "Embedded models.yaml is malformed at compile time: {e}"
                        ));
                    }
                    tracing::error!(
                        "Malformed {source} models.yaml: {e}. Falling through to lower-precedence layers."
                    );
                    continue;
                }
            };

            // Track version declared by this layer.
            let version = value
                .get("version")
                .and_then(|v| v.as_str())
                .map(String::from);
            declared_versions.push((*source, version));

            merge_layer_into(
                &mut merged,
                value,
                *source,
                &mut model_sources,
                &mut provider_sources,
            );
        }

        warn_on_version_mismatch(&declared_versions);

        let mut config: ModelConfiguration = serde_yaml::from_value(merged)
            .map_err(|e| anyhow!("Failed to deserialize merged model configuration: {e}"))?;

        for spec in &mut config.models {
            spec.source = model_sources
                .get(&spec.api_identifier)
                .copied()
                .unwrap_or_default();
        }
        for (name, prov) in &mut config.providers {
            prov.source = provider_sources.get(name).copied().unwrap_or_default();
        }

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

    /// Returns the merged model configuration.
    #[must_use]
    pub fn config(&self) -> &ModelConfiguration {
        &self.config
    }

    /// Returns the model specification for the given API identifier.
    #[must_use]
    pub fn get_model_spec(&self, api_identifier: &str) -> Option<&ModelSpec> {
        // Try exact match first
        if let Some(spec) = self.by_identifier.get(api_identifier) {
            return Some(spec);
        }

        // Try normalizing the identifier and looking up again
        self.find_model_by_normalized_id(api_identifier)
    }

    /// Returns the max output tokens for a model, with fallback to provider defaults.
    #[must_use]
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
        FALLBACK_MAX_OUTPUT_TOKENS
    }

    /// Returns the input context limit for a model, with fallback to provider defaults.
    #[must_use]
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
        FALLBACK_INPUT_CONTEXT
    }

    /// Returns whether the model supports structured JSON-schema output
    /// (`output_config.format`).
    ///
    /// Backs the schema-capability gate for the direct Anthropic and Bedrock
    /// backends (#1119). Resolves through the same identifier normalization as
    /// the other lookups, so Bedrock/region-prefixed forms map to the same
    /// [`ModelSpec`]. Any model not present in the catalog — or present but not
    /// flagged — returns `false`, keeping it on the YAML path rather than
    /// risking a `400` from an unsupported `output_config`.
    #[must_use]
    pub fn supports_structured_output(&self, api_identifier: &str) -> bool {
        self.get_model_spec(api_identifier)
            .is_some_and(|spec| spec.supports_structured_output)
    }

    /// Infers the provider from a model identifier.
    ///
    /// Consulted only as a fallback when an identifier does not match any
    /// known [`ModelSpec`]: the inferred provider selects which provider's
    /// [`DefaultConfig`] (from [`ProviderConfig::defaults`]) supplies the
    /// fallback limits. Recognised identifier shapes:
    ///
    /// - `claude`: starts with `claude`, or contains `anthropic` (covers the
    ///   Bedrock/AWS region-prefixed forms).
    /// - `openai`: starts with `gpt` or `chatgpt`, or is an `o<N>` reasoning
    ///   identifier (`o1-mini`, `o3`, `o4-mini`, …).
    /// - `gemini`: starts with `gemini`.
    ///
    /// Returns `None` when the provider cannot be inferred, in which case the
    /// caller applies the ultimate hard-coded fallback.
    fn infer_provider(&self, api_identifier: &str) -> Option<String> {
        let id = api_identifier;
        if id.starts_with("claude") || id.contains("anthropic") {
            Some("claude".to_string())
        } else if id.starts_with("gpt") || id.starts_with("chatgpt") || is_openai_reasoning_id(id) {
            Some("openai".to_string())
        } else if id.starts_with("gemini") {
            Some("gemini".to_string())
        } else {
            None
        }
    }

    /// Finds a model by normalizing the identifier and performing an exact lookup.
    ///
    /// Handles Bedrock-style (`us.anthropic.claude-3-7-sonnet-20250219-v1:0`),
    /// AWS-style (`anthropic.claude-3-haiku-20240307-v1:0`), and standard identifiers.
    fn find_model_by_normalized_id(&self, api_identifier: &str) -> Option<&ModelSpec> {
        let core_identifier = self.extract_core_model_identifier(api_identifier);
        self.by_identifier.get(&core_identifier)
    }

    /// Extracts the core model identifier from various formats.
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

    /// Checks if a model is legacy.
    #[must_use]
    pub fn is_legacy_model(&self, api_identifier: &str) -> bool {
        self.get_model_spec(api_identifier)
            .is_some_and(|spec| spec.legacy)
    }

    /// Returns all available models.
    #[must_use]
    pub fn get_all_models(&self) -> &[ModelSpec] {
        &self.config.models
    }

    /// Returns models filtered by provider.
    #[must_use]
    pub fn get_models_by_provider(&self, provider: &str) -> Vec<&ModelSpec> {
        self.by_provider
            .get(provider)
            .map(|models| models.iter().collect())
            .unwrap_or_default()
    }

    /// Returns models filtered by provider and tier.
    #[must_use]
    pub fn get_models_by_provider_and_tier(&self, provider: &str, tier: &str) -> Vec<&ModelSpec> {
        self.get_models_by_provider(provider)
            .into_iter()
            .filter(|model| model.tier == tier)
            .collect()
    }

    /// Returns the default model identifier for a provider, as defined in `models.yaml`.
    #[must_use]
    pub fn get_default_model(&self, provider: &str) -> Option<&str> {
        self.config
            .providers
            .get(provider)
            .map(|p| p.default_model.as_str())
    }

    /// Returns the provider configuration.
    #[must_use]
    pub fn get_provider_config(&self, provider: &str) -> Option<&ProviderConfig> {
        self.config.providers.get(provider)
    }

    /// Returns tier information for a provider.
    #[must_use]
    pub fn get_tier_info(&self, provider: &str, tier: &str) -> Option<&TierInfo> {
        self.config.providers.get(provider)?.tiers.get(tier)
    }

    /// Returns the beta headers for a model.
    #[must_use]
    pub fn get_beta_headers(&self, api_identifier: &str) -> &[BetaHeader] {
        self.get_model_spec(api_identifier)
            .map(|spec| spec.beta_headers.as_slice())
            .unwrap_or_default()
    }

    /// Returns the max output tokens for a model with a specific beta header active.
    #[must_use]
    pub fn get_max_output_tokens_with_beta(&self, api_identifier: &str, beta_value: &str) -> usize {
        if let Some(spec) = self.get_model_spec(api_identifier) {
            if let Some(bh) = spec.beta_headers.iter().find(|b| b.value == beta_value) {
                if let Some(max) = bh.max_output_tokens {
                    return max;
                }
            }
            return spec.max_output_tokens;
        }
        self.get_max_output_tokens(api_identifier)
    }

    /// Returns the input context for a model with a specific beta header active.
    #[must_use]
    pub fn get_input_context_with_beta(&self, api_identifier: &str, beta_value: &str) -> usize {
        if let Some(spec) = self.get_model_spec(api_identifier) {
            if let Some(bh) = spec.beta_headers.iter().find(|b| b.value == beta_value) {
                if let Some(ctx) = bh.input_context {
                    return ctx;
                }
            }
            return spec.input_context;
        }
        self.get_input_context(api_identifier)
    }
}

/// Returns `true` for OpenAI reasoning-series identifiers: a leading `o`
/// immediately followed by a digit (`o1-mini`, `o3`, `o4-mini`, and any
/// future `o<N>` variant). Kept separate from the `gpt`/`chatgpt` prefixes so
/// the reasoning family is matched without also swallowing unrelated
/// identifiers that merely begin with `o`.
fn is_openai_reasoning_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next() == Some('o') && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// Default project-local catalog path: `<cwd>/.omni-dev/models.yaml`.
fn default_project_path() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .map(|cwd| cwd.join(".omni-dev").join("models.yaml"))
}

/// Default user-level catalog path: `~/.omni-dev/models.yaml`.
fn default_user_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".omni-dev").join("models.yaml"))
}

/// Reads `path` if it exists. Returns `None` for missing files; logs and
/// returns `None` for read errors so the caller can fall through.
fn read_optional_yaml(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                "Failed to read {}: {e}. Falling through to lower-precedence layers.",
                path.display()
            );
            None
        }
    }
}

/// Merges a single layer's parsed YAML value into the accumulator.
///
/// The structure is treated specially at two top-level keys:
/// - `models`: a sequence merged by `api_identifier`. Existing entries are
///   deep-merged with the incoming entry; new entries are appended.
/// - `providers`: a mapping deep-merged per provider name (so a user file
///   can override e.g. `default_model` on the embedded `claude` provider
///   without having to re-declare every tier).
///
/// All other top-level keys (such as `version`) are last-writer-wins.
fn merge_layer_into(
    dest: &mut serde_yaml::Value,
    src: serde_yaml::Value,
    source: ModelSource,
    model_sources: &mut HashMap<String, ModelSource>,
    provider_sources: &mut HashMap<String, ModelSource>,
) {
    use serde_yaml::Value;

    let Value::Mapping(src_map) = src else {
        // Top-level isn't a mapping — treat the layer as a wholesale
        // replacement. (The embedded YAML is well-formed, so this is only
        // exercised by adversarial user input.)
        *dest = src;
        return;
    };

    if !matches!(dest, Value::Mapping(_)) {
        *dest = Value::Mapping(serde_yaml::Mapping::new());
    }
    let Value::Mapping(dest_map) = dest else {
        unreachable!("dest is a mapping after the check above");
    };

    for (k, v) in src_map {
        match k.as_str() {
            Some("models") => merge_models_into(dest_map, k, v, source, model_sources),
            Some("providers") => merge_providers_into(dest_map, k, v, source, provider_sources),
            _ => {
                dest_map.insert(k, v);
            }
        }
    }
}

fn merge_models_into(
    dest_map: &mut serde_yaml::Mapping,
    key: serde_yaml::Value,
    incoming: serde_yaml::Value,
    source: ModelSource,
    model_sources: &mut HashMap<String, ModelSource>,
) {
    use serde_yaml::Value;

    let Value::Sequence(incoming_seq) = incoming else {
        // Not a sequence — replace whatever is there.
        dest_map.insert(key, incoming);
        return;
    };

    let dest_value = dest_map
        .entry(key)
        .or_insert_with(|| Value::Sequence(Vec::new()));
    if !matches!(dest_value, Value::Sequence(_)) {
        *dest_value = Value::Sequence(Vec::new());
    }
    let Value::Sequence(dest_seq) = dest_value else {
        unreachable!("dest is a sequence after the check above");
    };

    for entry in incoming_seq {
        let api_id = entry
            .get("api_identifier")
            .and_then(|v| v.as_str())
            .map(String::from);

        let Some(api_id) = api_id else {
            tracing::warn!(
                "Skipping model entry without `api_identifier` from {source} models.yaml"
            );
            continue;
        };

        if let Some(existing) = dest_seq
            .iter_mut()
            .find(|e| e.get("api_identifier").and_then(serde_yaml::Value::as_str) == Some(&api_id))
        {
            deep_merge(existing, entry);
        } else {
            dest_seq.push(entry);
        }

        model_sources.insert(api_id, source);
    }
}

fn merge_providers_into(
    dest_map: &mut serde_yaml::Mapping,
    key: serde_yaml::Value,
    incoming: serde_yaml::Value,
    source: ModelSource,
    provider_sources: &mut HashMap<String, ModelSource>,
) {
    use serde_yaml::Value;

    let Value::Mapping(incoming_providers) = incoming else {
        dest_map.insert(key, incoming);
        return;
    };

    let dest_value = dest_map
        .entry(key)
        .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));
    if !matches!(dest_value, Value::Mapping(_)) {
        *dest_value = Value::Mapping(serde_yaml::Mapping::new());
    }
    let Value::Mapping(dest_providers) = dest_value else {
        unreachable!("dest is a mapping after the check above");
    };

    for (pname, pvalue) in incoming_providers {
        let pname_str = pname.as_str().map(String::from);

        if let Some(existing) = dest_providers.get_mut(&pname) {
            deep_merge(existing, pvalue);
        } else {
            dest_providers.insert(pname.clone(), pvalue);
        }

        if let Some(name) = pname_str {
            provider_sources.insert(name, source);
        }
    }
}

/// Recursive deep-merge: mappings are merged key-by-key, sequences and
/// scalars are replaced wholesale.
fn deep_merge(dest: &mut serde_yaml::Value, src: serde_yaml::Value) {
    use serde_yaml::Value;
    match (dest, src) {
        (Value::Mapping(d), Value::Mapping(s)) => {
            for (k, v) in s {
                if let Some(existing) = d.get_mut(&k) {
                    deep_merge(existing, v);
                } else {
                    d.insert(k, v);
                }
            }
        }
        (d, s) => *d = s,
    }
}

/// Logs a warning for each user-side layer whose `version` field differs
/// from the schema version this build understands.
fn warn_on_version_mismatch(declared: &[(ModelSource, Option<String>)]) {
    for (source, version) in declared {
        if matches!(source, ModelSource::Embedded) {
            continue;
        }
        match version {
            None => {
                tracing::warn!(
                    "{source} models.yaml has no `version:` field; assuming compatibility with schema version {MODELS_SCHEMA_VERSION}. Add `version: \"{MODELS_SCHEMA_VERSION}\"` to silence this warning."
                );
            }
            Some(v) if v == MODELS_SCHEMA_VERSION => {}
            Some(v) => {
                tracing::warn!(
                    "{source} models.yaml declares schema version {v}; this build understands {MODELS_SCHEMA_VERSION}. Continuing — unrecognised fields may be ignored."
                );
            }
        }
    }
}

/// Global model registry instance.
static MODEL_REGISTRY: OnceLock<ModelRegistry> = OnceLock::new();

/// Returns the global model registry instance.
#[must_use]
pub fn get_model_registry() -> &'static ModelRegistry {
    #[allow(clippy::expect_used)] // YAML is embedded via include_str! at compile time
    MODEL_REGISTRY.get_or_init(|| ModelRegistry::load().expect("Failed to load model registry"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn embedded_only() -> ModelRegistry {
        ModelRegistry::load_layered_from_paths(None, None, None).unwrap()
    }

    fn write_yaml(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn load_model_registry() {
        let registry = embedded_only();
        assert!(!registry.config.models.is_empty());
        assert!(registry.config.providers.contains_key("claude"));
        assert_eq!(
            registry.config.version.as_deref(),
            Some(MODELS_SCHEMA_VERSION)
        );
    }

    #[test]
    fn claude_model_lookup() {
        let registry = embedded_only();

        // Test legacy Claude 3 Opus
        let opus_spec = registry.get_model_spec("claude-3-opus-20240229");
        assert!(opus_spec.is_some());
        assert_eq!(opus_spec.unwrap().max_output_tokens, 4096);
        assert_eq!(opus_spec.unwrap().provider, "claude");
        assert!(registry.is_legacy_model("claude-3-opus-20240229"));

        // Test Claude 4.5 Sonnet (current generation)
        let sonnet45_tokens = registry.get_max_output_tokens("claude-sonnet-4-5-20250929");
        assert_eq!(sonnet45_tokens, 64000);

        // Test legacy Claude 4 Sonnet
        let sonnet4_tokens = registry.get_max_output_tokens("claude-sonnet-4-20250514");
        assert_eq!(sonnet4_tokens, 64000);
        assert!(registry.is_legacy_model("claude-sonnet-4-20250514"));

        // Test unknown model falls back to provider defaults
        let unknown_tokens = registry.get_max_output_tokens("claude-unknown-model");
        assert_eq!(unknown_tokens, 4096); // Should use Claude provider defaults
    }

    #[test]
    fn unknown_provider_uses_ultimate_fallback() {
        let registry = embedded_only();

        // Unknown identifier with no recognisable provider → ultimate fallback.
        assert_eq!(
            registry.get_max_output_tokens("totally-unknown-vendor-x"),
            FALLBACK_MAX_OUTPUT_TOKENS
        );
        assert_eq!(
            registry.get_input_context("totally-unknown-vendor-x"),
            FALLBACK_INPUT_CONTEXT
        );
    }

    #[test]
    fn unknown_openai_model_uses_openai_provider_defaults() {
        let registry = embedded_only();

        // Brand-new OpenAI identifiers the embedded catalog has not caught up
        // to yet resolve to the `openai` provider defaults (16384/128000), not
        // the ultimate hard-coded fallback. Covers the gpt-, chatgpt-, and
        // o<N>-reasoning identifier shapes.
        for id in ["gpt-6-ultra", "chatgpt-6-latest", "o5-preview"] {
            assert_eq!(
                registry.get_max_output_tokens(id),
                16384,
                "max_output_tokens for {id}"
            );
            assert_eq!(
                registry.get_input_context(id),
                128_000,
                "input_context for {id}"
            );
        }
    }

    #[test]
    fn unknown_gemini_model_uses_gemini_provider_defaults() {
        let registry = embedded_only();

        // Unknown Gemini identifier resolves to the `gemini` provider defaults
        // (8192/1048576) rather than the ultimate hard-coded fallback.
        let id = "gemini-9-pro";
        assert_eq!(registry.get_max_output_tokens(id), 8192);
        assert_eq!(registry.get_input_context(id), 1_048_576);
    }

    #[test]
    fn provider_filtering() {
        let registry = embedded_only();

        let claude_models = registry.get_models_by_provider("claude");
        assert!(!claude_models.is_empty());

        let fast_claude_models = registry.get_models_by_provider_and_tier("claude", "fast");
        assert!(!fast_claude_models.is_empty());

        let tier_info = registry.get_tier_info("claude", "fast");
        assert!(tier_info.is_some());
    }

    #[test]
    fn provider_config() {
        let registry = embedded_only();

        let claude_config = registry.get_provider_config("claude");
        assert!(claude_config.is_some());
        assert_eq!(claude_config.unwrap().name, "Anthropic Claude");
    }

    #[test]
    fn default_model_per_provider() {
        let registry = embedded_only();

        assert_eq!(
            registry.get_default_model("claude"),
            Some("claude-sonnet-5")
        );
        assert_eq!(registry.get_default_model("openai"), Some("gpt-5-mini"));
        assert_eq!(
            registry.get_default_model("gemini"),
            Some("gemini-2.5-flash")
        );
        assert_eq!(registry.get_default_model("nonexistent"), None);
    }

    #[test]
    fn normalized_id_matching() {
        let registry = embedded_only();

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

        // Test exact match still works for Claude 4.5 Sonnet
        let exact_sonnet45 = "claude-sonnet-4-5-20250929";
        let spec = registry.get_model_spec(exact_sonnet45);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().max_output_tokens, 64000);

        // Test legacy Claude 4 Sonnet
        let exact_sonnet4 = "claude-sonnet-4-20250514";
        let spec = registry.get_model_spec(exact_sonnet4);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().max_output_tokens, 64000);
    }

    /// Structured-output support is flagged per-model in the catalog (#1119):
    /// recent models (the default `claude-sonnet-4-6` included) advertise it,
    /// while older models and unknown identifiers do not — so the schema path
    /// never risks a `400` on a model that can't honour `output_config`.
    #[test]
    fn supports_structured_output_gates_by_model() {
        let registry = embedded_only();

        // Flagged models — including the default and a Bedrock-style prefix.
        assert!(registry.supports_structured_output("claude-fable-5"));
        assert!(registry.supports_structured_output("claude-opus-4-8"));
        assert!(registry.supports_structured_output("claude-opus-4-7"));
        assert!(registry.supports_structured_output("claude-sonnet-5"));
        assert!(registry.supports_structured_output("claude-sonnet-4-6"));
        assert!(registry.supports_structured_output("claude-opus-4-6"));
        assert!(registry.supports_structured_output("claude-haiku-4-5-20251001"));
        assert!(registry.supports_structured_output("claude-opus-4-5-20251101"));
        assert!(
            registry.supports_structured_output("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            "Bedrock/region-prefixed forms must normalize to the flagged spec"
        );

        // Older models that would 400 on output_config keep the YAML path.
        assert!(!registry.supports_structured_output("claude-opus-4-1-20250805"));
        assert!(!registry.supports_structured_output("claude-3-opus-20240229"));
        assert!(!registry.supports_structured_output("claude-sonnet-4-20250514"));

        // Unknown identifiers are conservatively unsupported.
        assert!(!registry.supports_structured_output("totally-unknown-model"));
    }

    /// The current-generation Claude models carry a 1M context window natively,
    /// at standard pricing with no long-context premium — no beta header
    /// involved (#1334).
    #[test]
    fn current_generation_models_are_registered() {
        let registry = embedded_only();

        for id in [
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
        ] {
            let spec = registry
                .get_model_spec(id)
                .unwrap_or_else(|| panic!("{id} must be registered"));
            assert_eq!(spec.input_context, 1_000_000, "{id} context window");
            assert_eq!(spec.max_output_tokens, 128_000, "{id} max output");
            assert!(
                spec.beta_headers.is_empty(),
                "{id} exposes 1M natively and must not rely on a context beta header"
            );
            assert!(spec.input_token_price.is_some(), "{id} input price");
            assert!(spec.output_token_price.is_some(), "{id} output price");
            assert!(!spec.legacy, "{id} is current, not legacy");
        }

        // Sonnet 5 records the sticker rate; the schema cannot express the
        // time-bounded introductory price ($2/$10 through 2026-08-31).
        let sonnet5 = registry.get_model_spec("claude-sonnet-5").unwrap();
        assert_eq!(sonnet5.input_token_price, Some(3.0));
        assert_eq!(sonnet5.output_token_price, Some(15.0));
    }

    /// The 4.6 generation also exposes 1M context natively. These values were
    /// stale (200k + a beta header, and a 64k output cap on Sonnet 4.6) — see
    /// #1334.
    #[test]
    fn claude_4_6_limits_are_native_not_beta_gated() {
        let registry = embedded_only();

        assert_eq!(registry.get_input_context("claude-opus-4-6"), 1_000_000);
        assert_eq!(registry.get_input_context("claude-sonnet-4-6"), 1_000_000);
        assert_eq!(registry.get_max_output_tokens("claude-sonnet-4-6"), 128_000);
        assert_eq!(registry.get_max_output_tokens("claude-opus-4-6"), 128_000);
    }

    /// Anthropic's undated aliases must each have their own entry: identifier
    /// resolution is exact-match plus a narrow normalization that never maps an
    /// alias onto its dated form. Without an entry the lookup misses and the
    /// caller silently gets the provider fallback (4096 output tokens) instead
    /// of the model's real limits — which is what `ANTHROPIC_DEFAULT_HAIKU_MODEL`
    /// defaulting to `claude-haiku-4-5` used to do (#1334).
    #[test]
    fn undated_aliases_resolve_to_real_limits() {
        let registry = embedded_only();

        for (alias, dated) in [
            ("claude-haiku-4-5", "claude-haiku-4-5-20251001"),
            ("claude-sonnet-4-5", "claude-sonnet-4-5-20250929"),
            ("claude-opus-4-5", "claude-opus-4-5-20251101"),
        ] {
            assert_eq!(
                registry.get_max_output_tokens(alias),
                registry.get_max_output_tokens(dated),
                "{alias} must resolve to the same output cap as {dated}, \
                 not the provider fallback"
            );
            assert_eq!(
                registry.get_input_context(alias),
                registry.get_input_context(dated),
                "{alias} context window must match {dated}"
            );
        }

        // The specific regression: the fallback would have been 4096.
        assert_eq!(registry.get_max_output_tokens("claude-haiku-4-5"), 64_000);
    }

    /// `retires` records an *announced* date only. Absence means "no published
    /// date" — never "not retiring" — which is why deprecated-but-undated models
    /// leave it unset (#1334).
    #[test]
    fn retires_records_announced_dates_only() {
        let registry = embedded_only();

        let opus_41 = registry.get_model_spec("claude-opus-4-1-20250805").unwrap();
        assert_eq!(opus_41.retires.as_deref(), Some("2026-08-05"));
        assert!(opus_41.legacy);

        // Retired models keep a past date; they are retained purely so Bedrock
        // identifier normalization still resolves them (ADR-0011).
        assert_eq!(
            registry
                .get_model_spec("claude-3-opus-20240229")
                .unwrap()
                .retires
                .as_deref(),
            Some("2026-01-05")
        );

        // Deprecated with retirement still TBD — no date invented.
        for id in ["claude-opus-4-20250514", "claude-sonnet-4-20250514"] {
            let spec = registry.get_model_spec(id).unwrap();
            assert!(spec.legacy, "{id} is deprecated");
            assert!(
                spec.retires.is_none(),
                "{id} has no announced retirement date; absence must not be \
                 filled in with a guess"
            );
        }

        // Current models are not retiring.
        assert!(registry
            .get_model_spec("claude-sonnet-5")
            .unwrap()
            .retires
            .is_none());
    }

    #[test]
    fn extract_core_model_identifier() {
        let registry = embedded_only();

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

    #[test]
    fn beta_header_lookups() {
        let registry = embedded_only();

        // Opus 4.6 base limits — 1M context is native on this generation, so
        // the base already grants what the context beta used to unlock (#1334).
        assert_eq!(registry.get_max_output_tokens("claude-opus-4-6"), 128_000);
        assert_eq!(registry.get_input_context("claude-opus-4-6"), 1_000_000);

        // The context beta is now a no-op for limits: it resolves to the same
        // 1M the base grants. The entry is retained only so that
        // `validate_beta_header` keeps accepting the flag from callers who
        // still pass it — hence it must still resolve, not vanish.
        assert_eq!(
            registry.get_input_context_with_beta("claude-opus-4-6", "context-1m-2025-08-07"),
            1_000_000
        );
        // max_output_tokens unchanged with context beta
        assert_eq!(
            registry.get_max_output_tokens_with_beta("claude-opus-4-6", "context-1m-2025-08-07"),
            128_000
        );

        // Sonnet 3.7 with output-128k beta
        assert_eq!(
            registry.get_max_output_tokens_with_beta(
                "claude-3-7-sonnet-20250219",
                "output-128k-2025-02-19"
            ),
            128_000
        );

        // Sonnet 3.7 base max_output_tokens without beta
        assert_eq!(
            registry.get_max_output_tokens("claude-3-7-sonnet-20250219"),
            64000
        );

        // Beta headers accessor
        let headers = registry.get_beta_headers("claude-opus-4-6");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].key, "anthropic-beta");
        assert_eq!(headers[0].value, "context-1m-2025-08-07");

        // Sonnet 3.7 has two beta headers
        let headers = registry.get_beta_headers("claude-3-7-sonnet-20250219");
        assert_eq!(headers.len(), 2);

        // Model without beta headers returns empty slice
        let headers = registry.get_beta_headers("claude-3-haiku-20240307");
        assert!(headers.is_empty());

        // Unknown model returns empty slice
        let headers = registry.get_beta_headers("unknown-model");
        assert!(headers.is_empty());
    }

    #[test]
    fn beta_lookups_for_unknown_model_fall_through_to_provider_defaults() {
        let registry = embedded_only();

        // Unknown model with arbitrary beta value: get_max_output_tokens_with_beta
        // and get_input_context_with_beta should both delegate to the no-beta
        // resolver, which in turn returns provider defaults for "claude-…".
        assert_eq!(
            registry
                .get_max_output_tokens_with_beta("claude-unknown-model", "context-1m-2025-08-07"),
            4096
        );
        assert_eq!(
            registry.get_input_context_with_beta("claude-unknown-model", "context-1m-2025-08-07"),
            200_000
        );
    }

    #[test]
    fn embedded_models_default_to_embedded_source() {
        let registry = embedded_only();
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.source, ModelSource::Embedded);

        let provider = registry.get_provider_config("claude").unwrap();
        assert_eq!(provider.source, ModelSource::Embedded);
    }

    #[test]
    fn missing_user_and_project_files_fall_through_silently() {
        let dir = tempfile::tempdir().unwrap();
        let project_path = dir.path().join("missing-project.yaml");
        let user_path = dir.path().join("missing-user.yaml");
        let registry =
            ModelRegistry::load_layered_from_paths(Some(&project_path), Some(&user_path), None)
                .unwrap();

        // Behaviour identical to embedded-only.
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.source, ModelSource::Embedded);
        assert_eq!(spec.max_output_tokens, 128_000);
    }

    #[test]
    fn user_layer_overrides_embedded_entry() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "Claude Opus 4.6 (custom)"
    api_identifier: "claude-opus-4-6"
    max_output_tokens: 999999
    input_context: 200000
    generation: 4.6
    tier: "flagship"
"#,
        );

        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.max_output_tokens, 999_999);
        assert_eq!(spec.model, "Claude Opus 4.6 (custom)");
        assert_eq!(spec.source, ModelSource::User);
    }

    #[test]
    fn project_layer_takes_precedence_over_user_layer() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "From User"
    api_identifier: "claude-opus-4-6"
    max_output_tokens: 1
    input_context: 1
    generation: 4.6
    tier: "flagship"
"#,
        );
        let project = write_yaml(
            dir.path(),
            "project.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "From Project"
    api_identifier: "claude-opus-4-6"
    max_output_tokens: 2
    input_context: 2
    generation: 4.6
    tier: "flagship"
"#,
        );

        let registry =
            ModelRegistry::load_layered_from_paths(Some(&project), Some(&user), None).unwrap();
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.model, "From Project");
        assert_eq!(spec.max_output_tokens, 2);
        assert_eq!(spec.source, ModelSource::Project);
    }

    #[test]
    fn additive_user_entry_is_appended() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "Claude Custom Future"
    api_identifier: "claude-future-9000"
    max_output_tokens: 250000
    input_context: 5000000
    generation: 9.0
    tier: "flagship"
"#,
        );

        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        let spec = registry.get_model_spec("claude-future-9000").unwrap();
        assert_eq!(spec.max_output_tokens, 250_000);
        assert_eq!(spec.input_context, 5_000_000);
        assert_eq!(spec.source, ModelSource::User);

        // And a pre-existing model is still present, sourced from embedded.
        let opus = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(opus.source, ModelSource::Embedded);
    }

    #[test]
    fn provider_fields_can_be_partially_overridden() {
        let dir = tempfile::tempdir().unwrap();
        // User only changes claude.default_model. Other fields (tiers,
        // defaults, api_base, name) must be preserved from the embedded layer.
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
providers:
  claude:
    default_model: "claude-opus-4-6"
"#,
        );

        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        let claude = registry.get_provider_config("claude").unwrap();
        assert_eq!(claude.default_model, "claude-opus-4-6");
        // Embedded fields must survive the partial override.
        assert_eq!(claude.name, "Anthropic Claude");
        assert_eq!(claude.api_base, "https://api.anthropic.com/v1");
        assert!(claude.tiers.contains_key("flagship"));
        // Provider source reflects the most-recent contributing layer.
        assert_eq!(claude.source, ModelSource::User);
    }

    #[test]
    fn malformed_user_yaml_logs_and_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            "this: is: definitely: not: valid: yaml: [unbalanced",
        );

        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        // Embedded catalog is intact.
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.source, ModelSource::Embedded);
        assert_eq!(spec.max_output_tokens, 128_000);
    }

    #[test]
    fn override_path_short_circuits_user_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "From User"
    api_identifier: "claude-opus-4-6"
    max_output_tokens: 1
    input_context: 1
    generation: 4.6
    tier: "flagship"
"#,
        );
        let project = write_yaml(
            dir.path(),
            "project.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "From Project"
    api_identifier: "claude-opus-4-6"
    max_output_tokens: 2
    input_context: 2
    generation: 4.6
    tier: "flagship"
"#,
        );
        let override_file = write_yaml(
            dir.path(),
            "override.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "From Override"
    api_identifier: "claude-opus-4-6"
    max_output_tokens: 3
    input_context: 3
    generation: 4.6
    tier: "flagship"
"#,
        );

        let registry = ModelRegistry::load_layered_from_paths(
            Some(&project),
            Some(&user),
            Some(&override_file),
        )
        .unwrap();
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.model, "From Override");
        assert_eq!(spec.max_output_tokens, 3);
        assert_eq!(spec.source, ModelSource::Override);
    }

    #[test]
    fn missing_override_path_falls_back_to_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.yaml");
        let registry = ModelRegistry::load_layered_from_paths(None, None, Some(&missing)).unwrap();
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.source, ModelSource::Embedded);
    }

    #[test]
    fn version_mismatch_is_warned_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "9999"
models:
  - provider: "claude"
    model: "From Future"
    api_identifier: "claude-future-9000"
    max_output_tokens: 1
    input_context: 1
    generation: 9.0
    tier: "flagship"
"#,
        );
        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        // Loaded successfully despite version mismatch.
        assert!(registry.get_model_spec("claude-future-9000").is_some());
    }

    #[test]
    fn missing_version_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
models:
  - provider: "claude"
    model: "Versionless"
    api_identifier: "claude-versionless"
    max_output_tokens: 1
    input_context: 1
    generation: 1.0
    tier: "flagship"
"#,
        );
        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        assert!(registry.get_model_spec("claude-versionless").is_some());
    }

    #[test]
    fn model_entry_without_api_identifier_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "No Id"
    max_output_tokens: 1
    input_context: 1
    generation: 1.0
    tier: "flagship"
"#,
        );
        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        // Registry still loads; embedded catalog unchanged.
        let opus = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(opus.source, ModelSource::Embedded);
    }

    #[test]
    fn model_source_display() {
        assert_eq!(ModelSource::Embedded.to_string(), "embedded");
        assert_eq!(ModelSource::User.to_string(), "user");
        assert_eq!(ModelSource::Project.to_string(), "project");
        assert_eq!(ModelSource::Override.to_string(), "override");
    }

    #[test]
    fn embedded_yaml_must_not_be_malformed() {
        // Sanity-check: a malformed embedded layer would be a hard error.
        let layers = [(ModelSource::Embedded, "::: not yaml :::".to_string())];
        let result = ModelRegistry::from_layers(&layers);
        assert!(result.is_err());
    }

    #[test]
    fn user_layer_with_scalar_top_level_returns_error() {
        // Adversarial: user YAML root is a string, not a mapping. The
        // wholesale-replacement branch in `merge_layer_into` discards the
        // embedded mapping; deserialise then fails cleanly.
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(dir.path(), "user.yaml", "\"just a string\"\n");
        let result = ModelRegistry::load_layered_from_paths(None, Some(&user), None);
        assert!(result.is_err());
    }

    #[test]
    fn user_layer_with_non_sequence_models_returns_error() {
        // Adversarial: `models: 42` triggers the non-sequence branch in
        // `merge_models_into`, which writes the scalar through. The final
        // `from_value` fails because `models` must be a sequence.
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models: 42
"#,
        );
        let result = ModelRegistry::load_layered_from_paths(None, Some(&user), None);
        assert!(result.is_err());
    }

    #[test]
    fn user_layer_with_non_mapping_providers_returns_error() {
        // Adversarial: `providers: 42` triggers the non-mapping branch in
        // `merge_providers_into`. The final `from_value` then fails.
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
providers: 42
"#,
        );
        let result = ModelRegistry::load_layered_from_paths(None, Some(&user), None);
        assert!(result.is_err());
    }

    #[test]
    fn deep_merge_inserts_new_keys_into_existing_mapping() {
        // Exercises the "key not in dest" branch of `deep_merge`. Adding a
        // new tier under `providers.claude.tiers` requires the merger to
        // *insert* (not overwrite) within an existing mapping.
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
providers:
  claude:
    tiers:
      experimental:
        description: "Experimental tier"
        use_cases: ["bleeding edge"]
"#,
        );
        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        let claude = registry.get_provider_config("claude").unwrap();
        // Embedded tiers preserved…
        assert!(claude.tiers.contains_key("flagship"));
        assert!(claude.tiers.contains_key("balanced"));
        assert!(claude.tiers.contains_key("fast"));
        // …and the new tier was inserted.
        let experimental = claude.tiers.get("experimental").unwrap();
        assert_eq!(experimental.description, "Experimental tier");
        assert_eq!(experimental.use_cases, vec!["bleeding edge".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn user_path_pointing_at_a_directory_logs_and_falls_through() {
        // A directory exists at the path, so `path.exists()` is true, but
        // `read_to_string` errors. The loader logs and falls through.
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("models.yaml");
        std::fs::create_dir(&bogus).unwrap();
        let registry = ModelRegistry::load_layered_from_paths(None, Some(&bogus), None).unwrap();
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.source, ModelSource::Embedded);
    }

    #[test]
    #[cfg(unix)]
    fn override_path_pointing_at_a_directory_warns_and_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("override.yaml");
        std::fs::create_dir(&bogus).unwrap();
        let registry = ModelRegistry::load_layered_from_paths(None, None, Some(&bogus)).unwrap();
        let spec = registry.get_model_spec("claude-opus-4-6").unwrap();
        assert_eq!(spec.source, ModelSource::Embedded);
    }

    #[test]
    fn project_layer_recovers_after_user_replaces_top_level_with_scalar() {
        // Layer-2 (user) wholesale-replaces the merged accumulator with a
        // scalar (early-return branch in `merge_layer_into`). Layer-3
        // (project) must hit the "dest is not a mapping" recovery branch
        // and rebuild a mapping before merging its own content. Project
        // must redeclare `providers` since the user layer wiped them.
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(dir.path(), "user.yaml", "\"junk\"\n");
        let project = write_yaml(
            dir.path(),
            "project.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "Project Rescue"
    api_identifier: "claude-rescue"
    max_output_tokens: 1
    input_context: 1
    generation: 1.0
    tier: "flagship"
providers:
  custom-provider:
    name: "Custom"
    api_base: "https://example.invalid"
    default_model: "custom-default"
    tiers: {}
    defaults:
      max_output_tokens: 100
      input_context: 1000
"#,
        );
        let registry =
            ModelRegistry::load_layered_from_paths(Some(&project), Some(&user), None).unwrap();
        // Project's model survives the user layer's top-level scalar wipe.
        let spec = registry.get_model_spec("claude-rescue").unwrap();
        assert_eq!(spec.source, ModelSource::Project);
    }

    #[test]
    fn project_layer_recovers_after_user_replaces_models_with_scalar() {
        // Layer-2 sets `models: 42`, replacing the embedded sequence with
        // a scalar. Layer-3 must trigger the "dest is not a sequence"
        // recovery branch in `merge_models_into` and rebuild the sequence.
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models: 42
"#,
        );
        let project = write_yaml(
            dir.path(),
            "project.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "Project Rescue"
    api_identifier: "claude-rescue"
    max_output_tokens: 1
    input_context: 1
    generation: 1.0
    tier: "flagship"
"#,
        );
        let registry =
            ModelRegistry::load_layered_from_paths(Some(&project), Some(&user), None).unwrap();
        let spec = registry.get_model_spec("claude-rescue").unwrap();
        assert_eq!(spec.source, ModelSource::Project);
    }

    #[test]
    fn project_layer_recovers_after_user_replaces_providers_with_scalar() {
        // Layer-2 sets `providers: 42`. Layer-3 must trigger the "dest is
        // not a mapping" recovery branch in `merge_providers_into`.
        let dir = tempfile::tempdir().unwrap();
        let user = write_yaml(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
providers: 42
"#,
        );
        let project = write_yaml(
            dir.path(),
            "project.yaml",
            r#"
version: "1"
providers:
  custom-provider:
    name: "Custom"
    api_base: "https://example.invalid"
    default_model: "custom-default"
    tiers: {}
    defaults:
      max_output_tokens: 100
      input_context: 1000
"#,
        );
        let registry =
            ModelRegistry::load_layered_from_paths(Some(&project), Some(&user), None).unwrap();
        let provider = registry.get_provider_config("custom-provider").unwrap();
        assert_eq!(provider.name, "Custom");
        assert_eq!(provider.source, ModelSource::Project);
    }

    #[test]
    fn empty_omni_dev_models_yaml_env_var_is_ignored() {
        // Exercises the `.filter(|s| !s.is_empty())` branch from `load()`
        // directly. The `load()` entry point is not safely callable from
        // a unit test because it consults a process-wide OnceLock.
        let resolved: Option<PathBuf> = Some(String::new())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        assert!(resolved.is_none());
        let resolved: Option<PathBuf> = Some("/some/path".to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        assert_eq!(resolved.as_deref(), Some(Path::new("/some/path")));
    }
}
