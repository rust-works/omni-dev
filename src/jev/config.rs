//! Credential and endpoint resolution for the Jev client.
//!
//! Reads through an injected [`EnvSource`](crate::utils::env::EnvSource) —
//! never `std::env::var` directly (STYLE-0028) — so production resolves via
//! `settings.json` fallback ([`crate::utils::settings::SettingsEnv`]) while
//! tests pass a pure `MapEnv` with no process-env mutation.

use anyhow::Result;

use crate::jev::error::JevError;
use crate::jev::protocol::DEFAULT_MODEL;
use crate::utils::env::{non_empty_var, EnvSource};
use crate::utils::secret::Secret;

/// Vendor-primary environment variable / settings key for the Jev API key,
/// matching TypeSafe's own SDK (mirrors `DATADOG_API_KEY`'s precedent of an
/// already-exported vendor variable just working).
pub const TYPESAFE_API_KEY: &str = "TYPESAFE_API_KEY";

/// omni-dev-specific override for the Jev API key, checked after
/// [`TYPESAFE_API_KEY`].
pub const OMNI_DEV_JEV_API_KEY: &str = "OMNI_DEV_JEV_API_KEY";

/// Environment variable / settings key overriding the Jev API base URL.
/// Defaults to [`DEFAULT_BASE_URL`] when unset.
pub const OMNI_DEV_JEV_BASE_URL: &str = "OMNI_DEV_JEV_BASE_URL";

/// Environment variable / settings key overriding the Jev model.
///
/// Deliberately distinct from the global `OMNI_DEV_MODEL`, which
/// [`JevConfig`] never reads — a user with `OMNI_DEV_MODEL` exported for the
/// AI backends must not have Jev calls silently redirected to an unrelated
/// model identifier.
pub const TYPESAFE_MODEL: &str = "TYPESAFE_MODEL";

/// Default Jev API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";

/// Resolved configuration for a [`crate::jev::client::JevClient`].
#[derive(Clone)]
pub struct JevConfig {
    /// The Jev API key (redacted in `Debug` output).
    pub api_key: Secret,
    /// The Jev API base URL, with any trailing slash trimmed.
    pub base_url: String,
    /// The Jev model to use, e.g. [`DEFAULT_MODEL`].
    pub model: String,
}

impl std::fmt::Debug for JevConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevConfig")
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

impl JevConfig {
    /// Resolves configuration from the process environment (with a
    /// `settings.json` fallback).
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(&crate::utils::settings::SettingsEnv::load())
    }

    /// [`from_env`](Self::from_env) over an injected [`EnvSource`].
    ///
    /// Tests pass a pure `MapEnv` to exercise precedence without mutating
    /// the process environment (STYLE-0028).
    pub(crate) fn from_env_with(env: &impl EnvSource) -> Result<Self> {
        let api_key = [TYPESAFE_API_KEY, OMNI_DEV_JEV_API_KEY]
            .into_iter()
            .find_map(|key| non_empty_var(env, key))
            .ok_or(JevError::CredentialsNotFound)?;

        let base_url = non_empty_var(env, OMNI_DEV_JEV_BASE_URL)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let model = non_empty_var(env, TYPESAFE_MODEL).unwrap_or_else(|| DEFAULT_MODEL.to_string());

        Ok(Self {
            api_key: api_key.into(),
            base_url,
            model,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    #[test]
    fn typesafe_api_key_wins_over_omni_dev_override() {
        let env = MapEnv::new()
            .with(TYPESAFE_API_KEY, "typesafe-key")
            .with(OMNI_DEV_JEV_API_KEY, "omni-dev-key");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.api_key.expose_secret(), "typesafe-key");
    }

    #[test]
    fn omni_dev_override_used_when_typesafe_key_unset() {
        let env = MapEnv::new().with(OMNI_DEV_JEV_API_KEY, "omni-dev-key");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.api_key.expose_secret(), "omni-dev-key");
    }

    #[test]
    fn empty_typesafe_key_counts_as_unset() {
        let env = MapEnv::new()
            .with(TYPESAFE_API_KEY, "")
            .with(OMNI_DEV_JEV_API_KEY, "omni-dev-key");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.api_key.expose_secret(), "omni-dev-key");
    }

    #[test]
    fn missing_api_key_is_credentials_not_found() {
        let env = MapEnv::new();
        let err = JevConfig::from_env_with(&env).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<JevError>(),
            Some(JevError::CredentialsNotFound)
        ));
    }

    #[test]
    fn base_url_defaults_when_unset() {
        let env = MapEnv::new().with(TYPESAFE_API_KEY, "k");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn base_url_override_trims_trailing_slash() {
        let env = MapEnv::new()
            .with(TYPESAFE_API_KEY, "k")
            .with(OMNI_DEV_JEV_BASE_URL, "http://proxy.example:8080/");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.base_url, "http://proxy.example:8080");
    }

    #[test]
    fn model_defaults_when_unset() {
        let env = MapEnv::new().with(TYPESAFE_API_KEY, "k");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.model, DEFAULT_MODEL);
    }

    #[test]
    fn model_override_resolves() {
        let env = MapEnv::new()
            .with(TYPESAFE_API_KEY, "k")
            .with(TYPESAFE_MODEL, "jev-1.13.0");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.model, "jev-1.13.0");
    }

    #[test]
    fn omni_dev_model_is_ignored() {
        let env = MapEnv::new()
            .with(TYPESAFE_API_KEY, "k")
            .with("OMNI_DEV_MODEL", "claude-opus-5");
        let config = JevConfig::from_env_with(&env).unwrap();
        assert_eq!(config.model, DEFAULT_MODEL);
    }

    #[test]
    fn debug_redacts_api_key() {
        let env = MapEnv::new().with(TYPESAFE_API_KEY, "sekret-value");
        let config = JevConfig::from_env_with(&env).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("sekret-value"));
        assert!(debug.contains("api_key: <redacted>"));
    }
}
