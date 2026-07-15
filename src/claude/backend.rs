//! Shared AI backend and model resolution.
//!
//! This module is the single source of truth for which AI backend an
//! invocation uses and which model it sends, consumed by both the client
//! factory ([`crate::claude::client::create_default_claude_client`]) and the
//! preflight credential check (`crate::utils::preflight`). Before it existed
//! the two sites duplicated the dispatch switch and had drifted apart
//! (issue #1118).
//!
//! Resolution reads the environment only through an
//! [`EnvSource`](crate::utils::env::EnvSource) (STYLE-0028); the production
//! callers pass `&SettingsEnv::load()`, so every variable below can also be
//! supplied from `~/.omni-dev/settings.json` `env` bundles or profiles.
//!
//! Backend selection precedence:
//!
//! 1. [`AI_BACKEND_ENV`] (`OMNI_DEV_AI_BACKEND`), set directly or via the
//!    global `--ai-backend` flag — wins outright, including the value
//!    `default`, which forces the direct Anthropic API even when a `USE_*`
//!    flag is set. An unknown value is a hard error.
//! 2. Legacy flags, first match wins: [`USE_OLLAMA_ENV`] → [`USE_OPENAI_ENV`]
//!    → [`USE_BEDROCK_ENV`] (each compared against the literal `true`).
//! 3. Otherwise the direct Anthropic API ([`AiBackend::Default`]).
//!
//! Model resolution stops at the first non-empty value: the explicit value
//! (CLI-independent callers such as MCP tools) → [`MODEL_ENV`]
//! (`OMNI_DEV_MODEL`, set by the global `--model` flag) → the backend
//! family's own variables → the registry default for the provider. The
//! Claude-family variables ([`CLAUDE_MODEL_ENV`], [`CLAUDE_CODE_MODEL_ENV`],
//! [`ANTHROPIC_MODEL_ENV`]) apply only to Claude-family backends; OpenAI and
//! Ollama read only their provider variable, so a Claude model id can never
//! leak into a non-Claude backend.

use anyhow::{anyhow, Result};

use crate::claude::model_config::ModelRegistry;
use crate::utils::env::EnvSource;

/// Env var selecting the AI backend (`default`, `claude-cli`, `openai`,
/// `ollama`, `bedrock`); set by the global `--ai-backend` flag.
pub const AI_BACKEND_ENV: &str = "OMNI_DEV_AI_BACKEND";
/// Env var carrying the backend-agnostic model override; set by the global
/// `--model` flag. Outranks every per-family model variable.
pub const MODEL_ENV: &str = "OMNI_DEV_MODEL";
/// Env var carrying a `key:value` beta header; set by the global
/// `--beta-header` flag.
pub const BETA_HEADER_ENV: &str = "OMNI_DEV_BETA_HEADER";
/// Highest-precedence Claude-family model variable.
pub const CLAUDE_MODEL_ENV: &str = "CLAUDE_MODEL";
/// Claude-family model variable, read after [`CLAUDE_MODEL_ENV`].
pub const CLAUDE_CODE_MODEL_ENV: &str = "CLAUDE_CODE_MODEL";
/// Claude-family model variable, read after [`CLAUDE_CODE_MODEL_ENV`].
pub const ANTHROPIC_MODEL_ENV: &str = "ANTHROPIC_MODEL";
/// Model variable for the OpenAI backend.
pub const OPENAI_MODEL_ENV: &str = "OPENAI_MODEL";
/// Model variable for the Ollama backend.
pub const OLLAMA_MODEL_ENV: &str = "OLLAMA_MODEL";
/// Legacy backend-selection flag for OpenAI (`true` to select).
pub const USE_OPENAI_ENV: &str = "USE_OPENAI";
/// Legacy backend-selection flag for Ollama (`true` to select).
pub const USE_OLLAMA_ENV: &str = "USE_OLLAMA";
/// Legacy backend-selection flag for AWS Bedrock (`true` to select).
pub const USE_BEDROCK_ENV: &str = "CLAUDE_CODE_USE_BEDROCK";

/// Hard fallback when the registry has no default for the `claude` provider.
///
/// Keep in sync with `providers.claude.default_model` in
/// `src/templates/models.yaml`; this only applies when that file fails to load.
const FALLBACK_CLAUDE_MODEL: &str = "claude-sonnet-5";
/// Hard fallback when the registry has no default for the `openai` provider.
const FALLBACK_OPENAI_MODEL: &str = "gpt-5-mini";
/// Default Ollama model; the registry has no `ollama` provider entry.
const FALLBACK_OLLAMA_MODEL: &str = "llama2";

/// The AI backend used by commands that invoke an AI model.
///
/// One enum serves both the `--ai-backend` CLI flag (via
/// [`clap::ValueEnum`]) and the resolved dispatch in the client factory and
/// preflight (STYLE-0019). Selection precedence is documented on
/// [`resolve_backend`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AiBackend {
    /// Direct HTTP to the Anthropic API. Selecting this explicitly overrides
    /// the legacy `USE_*` env vars.
    #[value(name = "default")]
    Default,
    /// Shell out to the `claude -p` CLI (reuses an existing Claude Code auth
    /// session). Equivalent to setting `OMNI_DEV_AI_BACKEND=claude-cli`.
    #[value(name = "claude-cli")]
    ClaudeCli,
    /// OpenAI API. Equivalent to setting `OMNI_DEV_AI_BACKEND=openai`
    /// (legacy selector: `USE_OPENAI=true`).
    #[value(name = "openai")]
    OpenAi,
    /// Local Ollama (or another OpenAI-compatible local server). Equivalent
    /// to setting `OMNI_DEV_AI_BACKEND=ollama` (legacy: `USE_OLLAMA=true`).
    #[value(name = "ollama")]
    Ollama,
    /// AWS Bedrock with Claude models. Equivalent to setting
    /// `OMNI_DEV_AI_BACKEND=bedrock` (legacy: `CLAUDE_CODE_USE_BEDROCK=true`).
    #[value(name = "bedrock")]
    Bedrock,
}

impl AiBackend {
    /// Returns the canonical [`AI_BACKEND_ENV`] value for this backend.
    pub fn env_value(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ClaudeCli => "claude-cli",
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
            Self::Bedrock => "bedrock",
        }
    }

    /// Parses an [`AI_BACKEND_ENV`] value.
    ///
    /// Accepts the canonical kebab-case values plus the legacy `claude_cli`
    /// underscore alias. Returns `None` for anything else.
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "claude-cli" | "claude_cli" => Some(Self::ClaudeCli),
            "openai" => Some(Self::OpenAi),
            "ollama" => Some(Self::Ollama),
            "bedrock" => Some(Self::Bedrock),
            _ => None,
        }
    }
}

/// Returns `var`'s value when it is set and non-empty.
///
/// The docs promise resolution "stopping at the first non-empty value", and
/// treating `VAR=` as unset lets users neutralise an exported variable for a
/// single invocation.
fn non_empty_var(env: &impl EnvSource, key: &str) -> Option<String> {
    env.var(key).filter(|v| !v.is_empty())
}

/// Resolves which AI backend to use from the environment.
///
/// [`AI_BACKEND_ENV`] wins outright when set (including `default`, which
/// forces the direct Anthropic API even when `USE_*` flags are set); an
/// unknown value is a hard error listing the valid values. When unset, the
/// legacy flags apply in order: [`USE_OLLAMA_ENV`] → [`USE_OPENAI_ENV`] →
/// [`USE_BEDROCK_ENV`], each selecting its backend when equal to `true`.
/// Otherwise the direct Anthropic API is used.
pub fn resolve_backend(env: &impl EnvSource) -> Result<AiBackend> {
    if let Some(raw) = non_empty_var(env, AI_BACKEND_ENV) {
        return AiBackend::from_env_value(&raw).ok_or_else(|| {
            anyhow!(
                "Unknown {AI_BACKEND_ENV} value '{raw}'. \
                 Valid values: default, claude-cli, openai, ollama, bedrock"
            )
        });
    }

    let flag_true = |key| env.var(key).is_some_and(|v| v == "true");
    if flag_true(USE_OLLAMA_ENV) {
        Ok(AiBackend::Ollama)
    } else if flag_true(USE_OPENAI_ENV) {
        Ok(AiBackend::OpenAi)
    } else if flag_true(USE_BEDROCK_ENV) {
        Ok(AiBackend::Bedrock)
    } else {
        Ok(AiBackend::Default)
    }
}

/// Resolves the model id for `backend`, stopping at the first non-empty
/// value.
///
/// Chain: `explicit` (callers with their own model parameter, e.g. MCP
/// tools) → [`MODEL_ENV`] → the backend family's variables
/// (Claude family: [`CLAUDE_MODEL_ENV`] → [`CLAUDE_CODE_MODEL_ENV`] →
/// [`ANTHROPIC_MODEL_ENV`]; OpenAI: [`OPENAI_MODEL_ENV`]; Ollama:
/// [`OLLAMA_MODEL_ENV`]) → the registry default for the provider → a
/// hard-coded fallback.
pub fn resolve_model(
    backend: AiBackend,
    explicit: Option<&str>,
    env: &impl EnvSource,
    registry: &ModelRegistry,
) -> String {
    if let Some(model) = explicit.filter(|m| !m.is_empty()) {
        return model.to_string();
    }
    if let Some(model) = non_empty_var(env, MODEL_ENV) {
        return model;
    }

    match backend {
        AiBackend::Default | AiBackend::ClaudeCli | AiBackend::Bedrock => {
            [CLAUDE_MODEL_ENV, CLAUDE_CODE_MODEL_ENV, ANTHROPIC_MODEL_ENV]
                .iter()
                .find_map(|key| non_empty_var(env, key))
                .unwrap_or_else(|| {
                    registry
                        .get_default_model("claude")
                        .unwrap_or(FALLBACK_CLAUDE_MODEL)
                        .to_string()
                })
        }
        AiBackend::OpenAi => non_empty_var(env, OPENAI_MODEL_ENV).unwrap_or_else(|| {
            registry
                .get_default_model("openai")
                .unwrap_or(FALLBACK_OPENAI_MODEL)
                .to_string()
        }),
        AiBackend::Ollama => non_empty_var(env, OLLAMA_MODEL_ENV)
            .unwrap_or_else(|| FALLBACK_OLLAMA_MODEL.to_string()),
    }
}

/// Parses a `--beta-header key:value` string into a `(key, value)` tuple.
pub fn parse_beta_header(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("Invalid --beta-header format '{s}'. Expected key:value"))?;
    Ok((k.to_string(), v.to_string()))
}

/// Resolves the beta header to send with AI API requests.
///
/// `explicit` (callers with their own parameter) wins; otherwise
/// [`BETA_HEADER_ENV`] (set by the global `--beta-header` flag) is parsed as
/// `key:value`. A set-but-malformed value is a hard error; unset (or empty)
/// resolves to `None`.
pub fn resolve_beta_header(
    explicit: Option<(String, String)>,
    env: &impl EnvSource,
) -> Result<Option<(String, String)>> {
    if explicit.is_some() {
        return Ok(explicit);
    }
    match non_empty_var(env, BETA_HEADER_ENV) {
        Some(raw) => parse_beta_header(&raw).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude::model_config::get_model_registry;
    use crate::test_support::env::MapEnv;

    #[test]
    fn env_value_round_trips_for_all_backends() {
        for backend in [
            AiBackend::Default,
            AiBackend::ClaudeCli,
            AiBackend::OpenAi,
            AiBackend::Ollama,
            AiBackend::Bedrock,
        ] {
            assert_eq!(
                AiBackend::from_env_value(backend.env_value()),
                Some(backend)
            );
        }
    }

    #[test]
    fn from_env_value_accepts_legacy_underscore_alias() {
        assert_eq!(
            AiBackend::from_env_value("claude_cli"),
            Some(AiBackend::ClaudeCli)
        );
    }

    #[test]
    fn from_env_value_rejects_unknown() {
        assert_eq!(AiBackend::from_env_value("gemini"), None);
        assert_eq!(AiBackend::from_env_value(""), None);
    }

    #[test]
    fn resolve_backend_defaults_to_direct_api() {
        assert_eq!(resolve_backend(&MapEnv::new()).unwrap(), AiBackend::Default);
    }

    #[test]
    fn resolve_backend_legacy_flags_in_order() {
        // Ollama beats OpenAI beats Bedrock (matches the pre-#1118 dispatch).
        let env = MapEnv::new()
            .with(USE_OLLAMA_ENV, "true")
            .with(USE_OPENAI_ENV, "true")
            .with(USE_BEDROCK_ENV, "true");
        assert_eq!(resolve_backend(&env).unwrap(), AiBackend::Ollama);

        let env = MapEnv::new()
            .with(USE_OPENAI_ENV, "true")
            .with(USE_BEDROCK_ENV, "true");
        assert_eq!(resolve_backend(&env).unwrap(), AiBackend::OpenAi);

        let env = MapEnv::new().with(USE_BEDROCK_ENV, "true");
        assert_eq!(resolve_backend(&env).unwrap(), AiBackend::Bedrock);
    }

    #[test]
    fn resolve_backend_legacy_flags_require_literal_true() {
        let env = MapEnv::new().with(USE_OLLAMA_ENV, "1");
        assert_eq!(resolve_backend(&env).unwrap(), AiBackend::Default);
    }

    #[test]
    fn resolve_backend_env_var_overrides_legacy_flags() {
        for (value, expected) in [
            ("default", AiBackend::Default),
            ("claude-cli", AiBackend::ClaudeCli),
            ("openai", AiBackend::OpenAi),
            ("bedrock", AiBackend::Bedrock),
        ] {
            let env = MapEnv::new()
                .with(AI_BACKEND_ENV, value)
                .with(USE_OLLAMA_ENV, "true");
            assert_eq!(resolve_backend(&env).unwrap(), expected, "value {value}");
        }
    }

    #[test]
    fn resolve_backend_unknown_value_is_hard_error() {
        let env = MapEnv::new().with(AI_BACKEND_ENV, "junk");
        let err = resolve_backend(&env).unwrap_err().to_string();
        assert!(err.contains("junk"), "unexpected error: {err}");
        assert!(err.contains("claude-cli"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_backend_empty_value_reads_as_unset() {
        let env = MapEnv::new()
            .with(AI_BACKEND_ENV, "")
            .with(USE_OLLAMA_ENV, "true");
        assert_eq!(resolve_backend(&env).unwrap(), AiBackend::Ollama);
    }

    #[test]
    fn resolve_model_explicit_wins_everywhere() {
        let env = MapEnv::new()
            .with(MODEL_ENV, "from-omni-dev-model")
            .with(CLAUDE_MODEL_ENV, "from-claude-model");
        let model = resolve_model(
            AiBackend::Default,
            Some("explicit"),
            &env,
            get_model_registry(),
        );
        assert_eq!(model, "explicit");
    }

    #[test]
    fn resolve_model_omni_dev_model_beats_family_vars() {
        for backend in [
            AiBackend::Default,
            AiBackend::ClaudeCli,
            AiBackend::OpenAi,
            AiBackend::Ollama,
            AiBackend::Bedrock,
        ] {
            let env = MapEnv::new()
                .with(MODEL_ENV, "global-model")
                .with(CLAUDE_MODEL_ENV, "claude-var")
                .with(OPENAI_MODEL_ENV, "openai-var")
                .with(OLLAMA_MODEL_ENV, "ollama-var");
            assert_eq!(
                resolve_model(backend, None, &env, get_model_registry()),
                "global-model",
                "backend {backend:?}"
            );
        }
    }

    #[test]
    fn resolve_model_claude_family_chain_order() {
        let registry = get_model_registry();
        for backend in [AiBackend::Default, AiBackend::ClaudeCli, AiBackend::Bedrock] {
            let env = MapEnv::new()
                .with(CLAUDE_MODEL_ENV, "a")
                .with(CLAUDE_CODE_MODEL_ENV, "b")
                .with(ANTHROPIC_MODEL_ENV, "c");
            assert_eq!(resolve_model(backend, None, &env, registry), "a");

            let env = MapEnv::new()
                .with(CLAUDE_CODE_MODEL_ENV, "b")
                .with(ANTHROPIC_MODEL_ENV, "c");
            assert_eq!(resolve_model(backend, None, &env, registry), "b");

            let env = MapEnv::new().with(ANTHROPIC_MODEL_ENV, "c");
            assert_eq!(resolve_model(backend, None, &env, registry), "c");

            assert_eq!(
                resolve_model(backend, None, &MapEnv::new(), registry),
                "claude-sonnet-5"
            );
        }
    }

    #[test]
    fn resolve_model_claude_vars_do_not_leak_into_openai_or_ollama() {
        let env = MapEnv::new()
            .with(CLAUDE_MODEL_ENV, "claude-opus-4-6")
            .with(ANTHROPIC_MODEL_ENV, "claude-opus-4-6");
        let registry = get_model_registry();
        assert_eq!(
            resolve_model(AiBackend::OpenAi, None, &env, registry),
            "gpt-5-mini"
        );
        assert_eq!(
            resolve_model(AiBackend::Ollama, None, &env, registry),
            "llama2"
        );
    }

    #[test]
    fn resolve_model_provider_vars() {
        let registry = get_model_registry();
        let env = MapEnv::new().with(OPENAI_MODEL_ENV, "gpt-4.1");
        assert_eq!(
            resolve_model(AiBackend::OpenAi, None, &env, registry),
            "gpt-4.1"
        );

        let env = MapEnv::new().with(OLLAMA_MODEL_ENV, "qwen3");
        assert_eq!(
            resolve_model(AiBackend::Ollama, None, &env, registry),
            "qwen3"
        );
    }

    #[test]
    fn resolve_model_skips_empty_values() {
        // The docs promise "stopping at the first non-empty value".
        let env = MapEnv::new()
            .with(CLAUDE_MODEL_ENV, "")
            .with(CLAUDE_CODE_MODEL_ENV, "b");
        assert_eq!(
            resolve_model(AiBackend::Default, None, &env, get_model_registry()),
            "b"
        );
    }

    #[test]
    fn resolve_beta_header_explicit_wins() {
        let env = MapEnv::new().with(BETA_HEADER_ENV, "env-key:env-value");
        let explicit = Some(("k".to_string(), "v".to_string()));
        let resolved = resolve_beta_header(explicit.clone(), &env).unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn resolve_beta_header_from_env() {
        let env = MapEnv::new().with(BETA_HEADER_ENV, "anthropic-beta:output-128k-2025-02-19");
        let resolved = resolve_beta_header(None, &env).unwrap();
        assert_eq!(
            resolved,
            Some((
                "anthropic-beta".to_string(),
                "output-128k-2025-02-19".to_string()
            ))
        );
    }

    #[test]
    fn resolve_beta_header_unset_is_none() {
        assert_eq!(resolve_beta_header(None, &MapEnv::new()).unwrap(), None);
    }

    #[test]
    fn resolve_beta_header_malformed_env_is_hard_error() {
        let env = MapEnv::new().with(BETA_HEADER_ENV, "no-colon-here");
        let err = resolve_beta_header(None, &env).unwrap_err().to_string();
        assert!(err.contains("no-colon-here"), "unexpected error: {err}");
    }

    #[test]
    fn parse_beta_header_valid() {
        let (key, value) = parse_beta_header("anthropic-beta:output-128k-2025-02-19").unwrap();
        assert_eq!(key, "anthropic-beta");
        assert_eq!(value, "output-128k-2025-02-19");
    }

    #[test]
    fn parse_beta_header_multiple_colons() {
        // Only splits on the first colon
        let (key, value) = parse_beta_header("key:value:with:colons").unwrap();
        assert_eq!(key, "key");
        assert_eq!(value, "value:with:colons");
    }

    #[test]
    fn parse_beta_header_missing_colon() {
        let result = parse_beta_header("no-colon-here");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("no-colon-here"));
    }

    #[test]
    fn parse_beta_header_empty_value() {
        let (key, value) = parse_beta_header("key:").unwrap();
        assert_eq!(key, "key");
        assert_eq!(value, "");
    }

    #[test]
    fn parse_beta_header_empty_key() {
        let (key, value) = parse_beta_header(":value").unwrap();
        assert_eq!(key, "");
        assert_eq!(value, "value");
    }
}
