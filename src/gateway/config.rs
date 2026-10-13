//! Gateway configuration, resolved from the environment (with `settings.json`
//! and `--profile` fallback) and overridden by command-line flags.
//!
//! Reads go through an injected [`EnvSource`] (STYLE-0028); the upstream key
//! and the gateway token go through the secret resolver (STYLE-0030), so both
//! accept `<NAME>_FILE` and `<NAME>_COMMAND` companions. One process serves
//! one upstream: per-upstream settings live in a profile per upstream.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use thiserror::Error;

use super::anthropic::Effort;
use super::flavor::{Flavor, UpstreamProtocol};
use super::models::ModelMap;
use super::policy::UnknownFields;
use crate::utils::env::{non_empty_var, EnvSource};
use crate::utils::secret::Secret;
use crate::utils::secret_env::secret_var;

/// Upstream base URL (required).
pub const UPSTREAM_URL_VAR: &str = "OMNI_DEV_GATEWAY_UPSTREAM_URL";
/// Upstream protocol: `chat`, `responses` or `anthropic`.
pub const UPSTREAM_PROTOCOL_VAR: &str = "OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL";
/// Upstream flavor: `generic`, `openai` or `fireworks`.
pub const FLAVOR_VAR: &str = "OMNI_DEV_GATEWAY_FLAVOR";
/// Upstream API key (secret).
pub const API_KEY_VAR: &str = "OMNI_DEV_GATEWAY_API_KEY";
/// Default upstream model.
pub const MODEL_VAR: &str = "OMNI_DEV_GATEWAY_MODEL";
/// Model map, `pattern=model,…`.
pub const MODEL_MAP_VAR: &str = "OMNI_DEV_GATEWAY_MODEL_MAP";
/// Inbound gateway token (secret).
pub const TOKEN_VAR: &str = "OMNI_DEV_GATEWAY_TOKEN";
/// Listening port on `127.0.0.1`.
pub const PORT_VAR: &str = "OMNI_DEV_GATEWAY_PORT";
/// Comma-separated `reasoning_effort` values the upstream model accepts.
pub const EFFORT_LEVELS_VAR: &str = "OMNI_DEV_GATEWAY_EFFORT_LEVELS";
/// Whether a leading inline `<think>` block is split out as reasoning.
pub const THINK_TAGS_VAR: &str = "OMNI_DEV_GATEWAY_THINK_TAGS";
/// `reject` (default) or `warn` for fields the gateway does not know.
pub const UNKNOWN_FIELDS_VAR: &str = "OMNI_DEV_GATEWAY_UNKNOWN_FIELDS";
/// Maximum concurrent upstream requests.
pub const MAX_CONCURRENT_VAR: &str = "OMNI_DEV_GATEWAY_MAX_CONCURRENT";

/// The default listening port, next to the browser bridge's 9998/9999.
pub const DEFAULT_PORT: u16 = 9997;
/// The default concurrency cap.
pub const DEFAULT_MAX_CONCURRENT: usize = 16;
/// The default inbound body limit (images arrive base64-encoded).
pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Why the gateway could not be configured.
#[derive(Debug, Error)]
pub enum GatewayConfigError {
    /// No upstream URL is configured; the daemon service stays idle.
    #[error("no upstream configured: set {UPSTREAM_URL_VAR} (or pass --upstream-url)")]
    NotConfigured,
}

/// Command-line overrides; `None`/empty keeps the environment's value.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    /// `--port`.
    pub port: Option<u16>,
    /// `--upstream-url`.
    pub upstream_url: Option<String>,
    /// `--upstream-protocol`.
    pub protocol: Option<UpstreamProtocol>,
    /// `--flavor`.
    pub flavor: Option<Flavor>,
    /// `--model`.
    pub model: Option<String>,
    /// `--map` (repeatable), appended after the environment's map so the
    /// flag's exact keys still win over the environment's globs.
    pub model_map: Vec<String>,
}

/// The resolved gateway configuration.
#[derive(Clone)]
pub struct GatewayConfig {
    /// Upstream base URL, without a trailing `/` or `/v1`.
    pub upstream_url: String,
    /// Upstream protocol.
    pub protocol: UpstreamProtocol,
    /// Upstream flavor.
    pub flavor: Flavor,
    /// Upstream key; `None` for servers that need none.
    pub api_key: Option<Secret>,
    /// Model mapping.
    pub models: ModelMap,
    /// Listening port (`0` = OS-assigned).
    pub port: u16,
    /// Reasoning effort levels the upstream model accepts.
    pub(crate) effort_levels: Vec<Effort>,
    /// Split a leading inline `<think>` block out of `content`.
    pub think_tags: bool,
    /// Unknown-field handling.
    pub unknown_fields: UnknownFields,
    /// Concurrency cap.
    pub max_concurrent: usize,
    /// Inbound body limit, in bytes.
    pub max_body_bytes: usize,
    /// Upstream connect timeout.
    pub connect_timeout: Duration,
    /// Upstream idle-read timeout (no bytes for this long fails the request).
    pub read_timeout: Duration,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("upstream_url", &self.upstream_url)
            .field("protocol", &self.protocol)
            .field("flavor", &self.flavor)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("models", &self.models)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl GatewayConfig {
    /// Resolves configuration from the process environment and settings.json.
    pub fn from_env(overrides: &ConfigOverrides) -> Result<Self> {
        Self::from_env_with(&crate::utils::settings::SettingsEnv::load(), overrides)
    }

    /// [`from_env`](Self::from_env) over an injected [`EnvSource`].
    pub fn from_env_with(env: &impl EnvSource, overrides: &ConfigOverrides) -> Result<Self> {
        let raw_url = overrides
            .upstream_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| non_empty_var(env, UPSTREAM_URL_VAR))
            .ok_or(GatewayConfigError::NotConfigured)?;
        let upstream_url = normalize_base_url(&raw_url)?;

        let protocol = match overrides.protocol {
            Some(p) => p,
            None => non_empty_var(env, UPSTREAM_PROTOCOL_VAR)
                .map(|v| UpstreamProtocol::parse(&v))
                .transpose()?
                .unwrap_or_default(),
        };
        let flavor = match overrides.flavor {
            Some(f) => f,
            None => non_empty_var(env, FLAVOR_VAR)
                .map(|v| Flavor::parse(&v))
                .transpose()?
                .unwrap_or_default(),
        };
        let api_key = secret_var(env, API_KEY_VAR)?;

        let default_model = overrides
            .model
            .clone()
            .filter(|m| !m.is_empty())
            .or_else(|| non_empty_var(env, MODEL_VAR));
        let mut entries: Vec<String> = overrides.model_map.clone();
        entries.extend(non_empty_var(env, MODEL_MAP_VAR));
        let models = ModelMap::new(&entries, default_model)
            .with_context(|| format!("invalid {MODEL_MAP_VAR} / --map"))?;

        let port = match overrides.port {
            Some(p) => p,
            None => port_from_env(env)?,
        };

        let effort_levels = match non_empty_var(env, EFFORT_LEVELS_VAR) {
            Some(raw) => parse_effort_levels(&raw)?,
            None => flavor.effort_levels().to_vec(),
        };
        let think_tags = parse_bool(env, THINK_TAGS_VAR, true)?;
        let unknown_fields = match non_empty_var(env, UNKNOWN_FIELDS_VAR)
            .as_deref()
            .map(str::trim)
        {
            None | Some("reject") => UnknownFields::Reject,
            Some("warn") => UnknownFields::Warn,
            Some(other) => bail!("{UNKNOWN_FIELDS_VAR}={other}: expected `reject` or `warn`"),
        };
        let max_concurrent = match non_empty_var(env, MAX_CONCURRENT_VAR) {
            Some(raw) => raw
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .with_context(|| format!("{MAX_CONCURRENT_VAR}={raw} is not a positive integer"))?,
            None => DEFAULT_MAX_CONCURRENT,
        };

        Ok(Self {
            upstream_url,
            protocol,
            flavor,
            api_key,
            models,
            port,
            effort_levels,
            think_tags,
            unknown_fields,
            max_concurrent,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            connect_timeout: Duration::from_secs(10),
            read_timeout: crate::claude::ai::request_timeout(),
        })
    }

    /// A one-line description of where the gateway sends requests.
    #[must_use]
    pub fn describe(&self) -> String {
        let default = self.models.default_model().unwrap_or("(none)");
        format!(
            "upstream: {} ({} protocol, {} flavor), default model: {default}, {} mapping rule(s)",
            self.upstream_url,
            self.protocol,
            self.flavor,
            self.models.rules().len()
        )
    }

    /// The URL clients set as `ANTHROPIC_BASE_URL`.
    #[must_use]
    pub fn base_url_for(port: u16) -> String {
        format!("http://127.0.0.1:{port}")
    }
}

/// The listening port from [`PORT_VAR`], else [`DEFAULT_PORT`]. Needs no
/// upstream, so `ai gateway env` can use it before one is configured.
pub fn port_from_env(env: &impl EnvSource) -> Result<u16> {
    non_empty_var(env, PORT_VAR)
        .map(|v| {
            v.trim()
                .parse::<u16>()
                .with_context(|| format!("{PORT_VAR}={v} is not a port number"))
        })
        .transpose()
        .map(|p| p.unwrap_or(DEFAULT_PORT))
}

/// Trims a trailing `/` and `/v1`, so `https://x/v1` and `https://x` both
/// work (the gateway appends `/v1/...` itself), and checks the scheme.
fn normalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    let parsed = reqwest::Url::parse(trimmed)
        .with_context(|| format!("{UPSTREAM_URL_VAR}={raw} is not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("{UPSTREAM_URL_VAR}={raw}: only http and https upstreams are supported");
    }
    Ok(trimmed.to_string())
}

fn parse_effort_levels(raw: &str) -> Result<Vec<Effort>> {
    let mut levels = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let Some(effort) = Effort::parse(token) else {
            bail!("{EFFORT_LEVELS_VAR}: unknown effort `{token}` (expected low, medium, high, xhigh, max)");
        };
        if !levels.contains(&effort) {
            levels.push(effort);
        }
    }
    if levels.is_empty() {
        bail!("{EFFORT_LEVELS_VAR} names no effort levels");
    }
    levels.sort();
    Ok(levels)
}

fn parse_bool(env: &impl EnvSource, key: &str, default: bool) -> Result<bool> {
    match non_empty_var(env, key).map(|v| v.trim().to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(false),
        Some(v) => bail!("{key}={v}: expected true or false"),
    }
}

// ── Gateway token ───────────────────────────────────────────────────

/// The persisted token file: `<data>/omni-dev/gateway.token`, `0600`.
pub fn token_path() -> Result<PathBuf> {
    Ok(crate::daemon::paths::runtime_dir()?.join("gateway.token"))
}

/// Resolves the inbound gateway token.
///
/// [`TOKEN_VAR`] (or its `_FILE`/`_COMMAND` companion) wins, else the
/// persisted token file, else a fresh token written to that file. Persisting keeps the token a client has configured stable across
/// restarts, and the standalone server and the daemon share it.
pub fn resolve_token(path: &Path) -> Result<String> {
    resolve_token_with(&crate::utils::settings::SettingsEnv::load(), path)
}

/// [`resolve_token`] over an injected [`EnvSource`].
pub fn resolve_token_with(env: &impl EnvSource, path: &Path) -> Result<String> {
    if let Some(value) = secret_var(env, TOKEN_VAR)? {
        let trimmed = value.expose_secret().trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if path.exists() {
        return crate::browser::auth::read_token_file(path);
    }
    let token = crate::browser::auth::generate_token();
    if let Some(parent) = path.parent() {
        crate::daemon::paths::ensure_dir_0700(parent)?;
    }
    crate::daemon::paths::write_file_0600(path, token.as_bytes())
        .with_context(|| format!("failed to write gateway token file {}", path.display()))?;
    Ok(token)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    fn env() -> MapEnv {
        MapEnv::new().with(UPSTREAM_URL_VAR, "https://api.fireworks.ai/inference/v1/")
    }

    #[test]
    fn missing_upstream_is_not_configured() {
        let err =
            GatewayConfig::from_env_with(&MapEnv::new(), &ConfigOverrides::default()).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<GatewayConfigError>(),
            Some(GatewayConfigError::NotConfigured)
        ));
    }

    #[test]
    fn defaults_and_url_normalization() {
        let config = GatewayConfig::from_env_with(&env(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.upstream_url, "https://api.fireworks.ai/inference");
        assert_eq!(config.protocol, UpstreamProtocol::Chat);
        assert_eq!(config.flavor, Flavor::Generic);
        assert!(config.api_key.is_none());
        assert_eq!(config.port, DEFAULT_PORT);
        assert!(config.think_tags);
        assert_eq!(config.unknown_fields, UnknownFields::Reject);
        assert_eq!(config.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(config.effort_levels, Flavor::Generic.effort_levels());
        let debug = format!("{config:?}");
        assert!(debug.contains("fireworks"));
    }

    #[test]
    fn environment_values_are_read() {
        let env = env()
            .with(UPSTREAM_PROTOCOL_VAR, "responses")
            .with(FLAVOR_VAR, "openai")
            .with(API_KEY_VAR, "sk-test")
            .with(MODEL_VAR, "gpt-5")
            .with(MODEL_MAP_VAR, "claude-haiku-*=gpt-5-mini")
            .with(PORT_VAR, "4000")
            .with(EFFORT_LEVELS_VAR, "high, low,low")
            .with(THINK_TAGS_VAR, "off")
            .with(UNKNOWN_FIELDS_VAR, "warn")
            .with(MAX_CONCURRENT_VAR, "3");
        let config = GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.protocol, UpstreamProtocol::Responses);
        assert_eq!(config.flavor, Flavor::Openai);
        assert_eq!(config.api_key.as_ref().unwrap().expose_secret(), "sk-test");
        assert_eq!(
            config.models.resolve("claude-haiku-4-5").unwrap(),
            "gpt-5-mini"
        );
        assert_eq!(config.models.resolve("claude-opus-5").unwrap(), "gpt-5");
        assert_eq!(config.port, 4000);
        assert_eq!(config.effort_levels, vec![Effort::Low, Effort::High]);
        assert!(!config.think_tags);
        assert_eq!(config.unknown_fields, UnknownFields::Warn);
        assert_eq!(config.max_concurrent, 3);
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-test"));
    }

    #[test]
    fn overrides_win_and_map_flags_come_first() {
        let env = env()
            .with(MODEL_MAP_VAR, "claude-x=env")
            .with(MODEL_VAR, "env-default");
        let overrides = ConfigOverrides {
            port: Some(0),
            upstream_url: Some("http://localhost:11434".into()),
            protocol: Some(UpstreamProtocol::Anthropic),
            flavor: Some(Flavor::Fireworks),
            model: Some("flag-default".into()),
            model_map: vec!["claude-x=flag".into()],
        };
        let config = GatewayConfig::from_env_with(&env, &overrides).unwrap();
        assert_eq!(config.upstream_url, "http://localhost:11434");
        assert_eq!(config.port, 0);
        assert_eq!(config.protocol, UpstreamProtocol::Anthropic);
        assert_eq!(config.flavor, Flavor::Fireworks);
        assert_eq!(config.models.resolve("claude-x").unwrap(), "flag");
        assert_eq!(config.models.resolve("claude-y").unwrap(), "flag-default");
    }

    #[test]
    fn the_api_key_accepts_a_file_companion() {
        let (_dir, path) = crate::test_support::env::secret_file("sk-from-file\n");
        let env = env().with(&format!("{API_KEY_VAR}_FILE"), &path);
        let config = GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.api_key.unwrap().expose_secret(), "sk-from-file");
    }

    #[test]
    fn invalid_values_are_errors() {
        for (key, value) in [
            (UPSTREAM_PROTOCOL_VAR, "grpc"),
            (FLAVOR_VAR, "bedrock"),
            (PORT_VAR, "99999"),
            (EFFORT_LEVELS_VAR, "huge"),
            (EFFORT_LEVELS_VAR, ","),
            (THINK_TAGS_VAR, "maybe"),
            (UNKNOWN_FIELDS_VAR, "panic"),
            (MAX_CONCURRENT_VAR, "0"),
            (MODEL_MAP_VAR, "nonsense"),
        ] {
            let result =
                GatewayConfig::from_env_with(&env().with(key, value), &ConfigOverrides::default());
            assert!(result.is_err(), "{key}={value}");
        }
        for url in ["not a url", "ftp://x"] {
            let env = MapEnv::new().with(UPSTREAM_URL_VAR, url);
            assert!(GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).is_err());
        }
    }

    #[test]
    fn base_url_is_loopback() {
        assert_eq!(GatewayConfig::base_url_for(9997), "http://127.0.0.1:9997");
    }

    #[test]
    fn the_token_comes_from_the_variable_then_the_file_then_is_generated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("gateway.token");
        let explicit = MapEnv::new().with(TOKEN_VAR, " tok ");
        assert_eq!(resolve_token_with(&explicit, &path).unwrap(), "tok");
        assert!(!path.exists());

        let generated = resolve_token_with(&MapEnv::new(), &path).unwrap();
        assert!(path.exists());
        assert_eq!(
            resolve_token_with(&MapEnv::new(), &path).unwrap(),
            generated
        );
        let blank = MapEnv::new().with(TOKEN_VAR, "  ");
        assert_eq!(resolve_token_with(&blank, &path).unwrap(), generated);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
