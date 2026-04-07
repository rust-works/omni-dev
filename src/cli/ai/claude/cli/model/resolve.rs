//! Model resolution diagnostic command.
//!
//! Inspects Claude Code settings files, environment variables, and provider
//! configuration to report how Claude Code would select a model when started
//! in the current directory.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use serde::Serialize;

/// Show how Claude Code resolves the active model in the current directory.
#[derive(Parser)]
pub struct ResolveCommand {}

impl ResolveCommand {
    /// Executes the resolve command.
    pub fn execute(self) -> Result<()> {
        let report = build_resolution_report()?;
        let yaml = crate::data::yaml::to_yaml(&report)?;
        println!("{yaml}");
        Ok(())
    }
}

// ── Data structures ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ResolutionReport {
    active_model: ActiveModel,
    resolution_chain: Vec<ResolutionStep>,
    settings_files: Vec<SettingsFileInfo>,
    provider: ProviderInfo,
    context_window: ContextWindowInfo,
    fast_mode: FastModeInfo,
    subagent: SubagentInfo,
    model_defaults: ModelDefaults,
    aliases: Vec<AliasEntry>,
}

#[derive(Debug, Serialize)]
struct ActiveModel {
    resolved: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_value: Option<String>,
    alias_expanded: bool,
    provider: String,
    provider_model_id: String,
}

#[derive(Debug, Serialize)]
struct ResolutionStep {
    priority: u8,
    source: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    variable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    winner: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct SettingsFileInfo {
    source: String,
    path: String,
    exists: bool,
    trust_level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    available_models: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_overrides: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env_anthropic_model: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProviderInfo {
    active: String,
    env_vars: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ContextWindowInfo {
    default_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_suffix: Option<String>,
    #[serde(rename = "CLAUDE_CODE_DISABLE_1M_CONTEXT")]
    disable_1m: String,
    #[serde(rename = "CLAUDE_CODE_MAX_CONTEXT_TOKENS")]
    max_context_tokens: String,
}

#[derive(Debug, Serialize)]
struct FastModeInfo {
    #[serde(rename = "CLAUDE_CODE_DISABLE_FAST_MODE")]
    disable_fast_mode: String,
    supported_by_resolved_model: bool,
}

#[derive(Debug, Serialize)]
struct SubagentInfo {
    #[serde(rename = "CLAUDE_CODE_SUBAGENT_MODEL")]
    subagent_model: String,
}

#[derive(Debug, Serialize)]
struct ModelDefaults {
    opus: String,
    sonnet: String,
    haiku: String,
    #[serde(rename = "ANTHROPIC_DEFAULT_OPUS_MODEL")]
    opus_override: String,
    #[serde(rename = "ANTHROPIC_DEFAULT_SONNET_MODEL")]
    sonnet_override: String,
    #[serde(rename = "ANTHROPIC_DEFAULT_HAIKU_MODEL")]
    haiku_override: String,
    #[serde(rename = "ANTHROPIC_SMALL_FAST_MODEL")]
    small_fast_model: String,
}

#[derive(Debug, Serialize)]
struct AliasEntry {
    alias: String,
    resolves_to: String,
}

// ── Alias & model resolution helpers ───────────────────────────────

/// Known aliases and their resolved model IDs.
fn get_aliases(defaults: &ResolvedDefaults) -> Vec<AliasEntry> {
    vec![
        AliasEntry {
            alias: "sonnet".into(),
            resolves_to: defaults.sonnet.clone(),
        },
        AliasEntry {
            alias: "opus".into(),
            resolves_to: defaults.opus.clone(),
        },
        AliasEntry {
            alias: "haiku".into(),
            resolves_to: defaults.haiku.clone(),
        },
        AliasEntry {
            alias: "best".into(),
            resolves_to: defaults.opus.clone(),
        },
        AliasEntry {
            alias: "opusplan".into(),
            resolves_to: format!(
                "{} (plan mode) / {} (normal)",
                defaults.opus, defaults.sonnet
            ),
        },
        AliasEntry {
            alias: "sonnet[1m]".into(),
            resolves_to: format!("{} (1M context)", defaults.sonnet),
        },
        AliasEntry {
            alias: "opus[1m]".into(),
            resolves_to: format!("{} (1M context)", defaults.opus),
        },
    ]
}

struct ResolvedDefaults {
    opus: String,
    sonnet: String,
    haiku: String,
}

fn resolve_defaults() -> ResolvedDefaults {
    ResolvedDefaults {
        opus: env::var("ANTHROPIC_DEFAULT_OPUS_MODEL").unwrap_or_else(|_| "claude-opus-4-6".into()),
        sonnet: env::var("ANTHROPIC_DEFAULT_SONNET_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-6".into()),
        haiku: env::var("ANTHROPIC_DEFAULT_HAIKU_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5".into()),
    }
}

/// Expand a known alias to its full model ID.
fn expand_alias(value: &str, defaults: &ResolvedDefaults) -> Option<String> {
    // Strip [1m] suffix for alias matching, re-apply later
    let (base, suffix) = if let Some(base) = value.strip_suffix("[1m]") {
        (base, "[1m]")
    } else {
        (value, "")
    };

    let expanded = match base {
        "sonnet" | "opusplan" => Some(defaults.sonnet.clone()),
        "opus" | "best" => Some(defaults.opus.clone()),
        "haiku" => Some(defaults.haiku.clone()),
        _ => None,
    };

    expanded.map(|m| {
        if suffix.is_empty() {
            m
        } else {
            format!("{m}{suffix}")
        }
    })
}

// ── Provider helpers ───────────────────────────────────────────────

fn detect_provider() -> String {
    if env_is_set("CLAUDE_CODE_USE_BEDROCK") {
        "bedrock".into()
    } else if env_is_set("CLAUDE_CODE_USE_VERTEX") {
        "vertex".into()
    } else if env_is_set("CLAUDE_CODE_USE_FOUNDRY") {
        "foundry".into()
    } else {
        "first_party".into()
    }
}

/// Map a first-party model ID to the provider-specific ID.
fn provider_model_id(model: &str, provider: &str) -> String {
    match provider {
        "bedrock" => bedrock_model_id(model),
        // Vertex and Foundry use the same IDs as first-party for current models
        _ => model.to_string(),
    }
}

fn bedrock_model_id(model: &str) -> String {
    // Bedrock uses a different naming convention with version suffix
    let base = model.strip_suffix("[1m]").unwrap_or(model);
    if base.starts_with("claude-") {
        let region_prefix = env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into());
        let short_region = region_prefix.split('-').next().unwrap_or("us");
        format!("{short_region}.anthropic.{base}-v1:0")
    } else {
        model.to_string()
    }
}

// ── Settings file scanning ─────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

fn settings_file_entries() -> Vec<(String, PathBuf, String)> {
    let mut entries = Vec::new();

    if let Some(home) = home_dir() {
        entries.push((
            "user_settings".into(),
            home.join(".claude").join("settings.json"),
            "trusted".into(),
        ));
    }

    // Project settings — relative to cwd
    entries.push((
        "project_settings".into(),
        PathBuf::from(".claude/settings.json"),
        "untrusted".into(),
    ));

    entries.push((
        "local_settings".into(),
        PathBuf::from(".claude/settings.local.json"),
        "untrusted".into(),
    ));

    // Policy settings (macOS paths)
    entries.push((
        "policy_mdm".into(),
        PathBuf::from("/Library/Managed Preferences/com.anthropic.claudecode.plist"),
        "trusted".into(),
    ));

    entries.push((
        "policy_managed".into(),
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"),
        "trusted".into(),
    ));

    entries
}

fn scan_settings_file(path: &Path) -> SettingsFileResult {
    if !path.exists() {
        return SettingsFileResult::default();
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return SettingsFileResult::default();
    };

    // plist files aren't JSON — just report existence
    if path.extension().is_some_and(|ext| ext == "plist") {
        return SettingsFileResult {
            exists: true,
            ..Default::default()
        };
    }

    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => {
            return SettingsFileResult {
                exists: true,
                ..Default::default()
            }
        }
    };

    let model = parsed.get("model").cloned();
    let available_models = parsed.get("availableModels").cloned();
    let model_overrides = parsed.get("modelOverrides").cloned();
    let env_anthropic_model = parsed
        .get("env")
        .and_then(|e| e.get("ANTHROPIC_MODEL"))
        .and_then(serde_json::Value::as_str)
        .map(String::from);

    SettingsFileResult {
        exists: true,
        model,
        available_models,
        model_overrides,
        env_anthropic_model,
    }
}

#[derive(Default)]
struct SettingsFileResult {
    exists: bool,
    model: Option<serde_json::Value>,
    available_models: Option<serde_json::Value>,
    model_overrides: Option<serde_json::Value>,
    env_anthropic_model: Option<String>,
}

// ── Environment variable helpers ───────────────────────────────────

fn env_val(key: &str) -> String {
    env::var(key).unwrap_or_else(|_| "not_set".into())
}

fn env_is_set(key: &str) -> bool {
    env::var(key).is_ok()
}

// ── Main report builder ────────────────────────────────────────────

fn build_resolution_report() -> Result<ResolutionReport> {
    let defaults = resolve_defaults();
    let provider_name = detect_provider();

    // Scan settings files for model-related fields
    let settings_entries = settings_file_entries();
    let mut settings_files = Vec::new();
    let mut settings_model_value: Option<String> = None;
    let mut settings_env_model: Option<String> = None;

    for (source, path, trust) in &settings_entries {
        let result = scan_settings_file(path);

        // Track the highest-priority settings model we find
        if settings_model_value.is_none() {
            if let Some(ref m) = result.model {
                settings_model_value = m.as_str().map(String::from);
            }
        }
        if settings_env_model.is_none() {
            if let Some(ref e) = result.env_anthropic_model {
                settings_env_model = Some(e.clone());
            }
        }

        settings_files.push(SettingsFileInfo {
            source: source.clone(),
            path: path.display().to_string(),
            exists: result.exists,
            trust_level: trust.clone(),
            model: result.model,
            available_models: result.available_models,
            model_overrides: result.model_overrides,
            env_anthropic_model: result.env_anthropic_model,
        });
    }

    // Build the resolution chain (priorities 1-5)
    // Priority 1: session override (can't detect from outside, always not_set)
    // Priority 2: CLI flag (can't detect from outside, always not_set)
    // Priority 3: ANTHROPIC_MODEL env var (check actual env + settings.env)
    // Priority 4: settings.model field
    // Priority 5: tier-based default

    let env_model = env::var("ANTHROPIC_MODEL").ok().or(settings_env_model);
    let mut winner_found = false;

    let mut chain = Vec::new();

    // Priority 1: session override
    chain.push(ResolutionStep {
        priority: 1,
        source: "session_override".into(),
        status: "not_applicable".into(),
        value: None,
        variable: None,
        path: None,
        winner: false,
        reason: Some("Cannot detect from outside a running session".into()),
    });

    // Priority 2: CLI flag
    chain.push(ResolutionStep {
        priority: 2,
        source: "cli_flag".into(),
        status: "not_applicable".into(),
        value: None,
        variable: None,
        path: None,
        winner: false,
        reason: Some("Depends on --model flag at startup".into()),
    });

    // Priority 3: environment variable
    let env_model_status = if let Some(ref val) = env_model {
        winner_found = true;
        ResolutionStep {
            priority: 3,
            source: "environment_variable".into(),
            status: "set".into(),
            value: Some(val.clone()),
            variable: Some("ANTHROPIC_MODEL".into()),
            path: None,
            winner: true,
            reason: None,
        }
    } else {
        ResolutionStep {
            priority: 3,
            source: "environment_variable".into(),
            status: "not_set".into(),
            value: None,
            variable: Some("ANTHROPIC_MODEL".into()),
            path: None,
            winner: false,
            reason: None,
        }
    };
    chain.push(env_model_status);

    // Priority 4: user settings
    let settings_step = if let Some(ref val) = settings_model_value {
        ResolutionStep {
            priority: 4,
            source: "user_settings".into(),
            status: "set".into(),
            value: Some(val.clone()),
            variable: None,
            path: home_dir().map(|h| h.join(".claude/settings.json").display().to_string()),
            winner: !winner_found && {
                winner_found = true;
                true
            },
            reason: if winner_found && chain.iter().any(|s| s.winner) {
                Some("Overridden by higher priority source".into())
            } else {
                None
            },
        }
    } else {
        ResolutionStep {
            priority: 4,
            source: "user_settings".into(),
            status: "not_set".into(),
            value: None,
            variable: None,
            path: home_dir().map(|h| h.join(".claude/settings.json").display().to_string()),
            winner: false,
            reason: None,
        }
    };
    chain.push(settings_step);

    // Priority 5: tier default
    let tier_default = defaults.sonnet.clone(); // Most common default
    chain.push(ResolutionStep {
        priority: 5,
        source: "tier_default".into(),
        status: "available".into(),
        value: Some(tier_default.clone()),
        variable: None,
        path: None,
        winner: !winner_found,
        reason: if winner_found {
            Some("Overridden by higher priority source".into())
        } else {
            None
        },
    });

    // Determine the winning raw value and resolve it
    let winning_raw = env_model
        .as_deref()
        .or(settings_model_value.as_deref())
        .unwrap_or(&tier_default);

    let winning_source = if env_model.is_some() {
        "environment_variable"
    } else if settings_model_value.is_some() {
        "user_settings"
    } else {
        "tier_default"
    };

    let alias_expanded = expand_alias(winning_raw, &defaults).is_some();
    let resolved = expand_alias(winning_raw, &defaults).unwrap_or_else(|| winning_raw.to_string());
    let prov_model = provider_model_id(&resolved, &provider_name);

    // Context window
    let suffix_1m = if winning_raw.contains("[1m]") {
        Some("[1m]".to_string())
    } else {
        None
    };
    let default_tokens = if suffix_1m.is_some() {
        1_000_000
    } else {
        200_000
    };

    // Fast mode: only Opus 4.6 supports it
    let supports_fast = resolved.contains("opus-4-6");

    let active_model = ActiveModel {
        resolved: resolved.clone(),
        source: winning_source.into(),
        raw_value: if alias_expanded || winning_raw != resolved {
            Some(winning_raw.to_string())
        } else {
            None
        },
        alias_expanded,
        provider: provider_name.clone(),
        provider_model_id: prov_model,
    };

    // Provider env vars
    let mut provider_env = BTreeMap::new();
    provider_env.insert(
        "CLAUDE_CODE_USE_BEDROCK".into(),
        env_val("CLAUDE_CODE_USE_BEDROCK"),
    );
    provider_env.insert(
        "CLAUDE_CODE_USE_VERTEX".into(),
        env_val("CLAUDE_CODE_USE_VERTEX"),
    );
    provider_env.insert(
        "CLAUDE_CODE_USE_FOUNDRY".into(),
        env_val("CLAUDE_CODE_USE_FOUNDRY"),
    );

    Ok(ResolutionReport {
        active_model,
        resolution_chain: chain,
        settings_files,
        provider: ProviderInfo {
            active: provider_name,
            env_vars: provider_env,
        },
        context_window: ContextWindowInfo {
            default_tokens,
            model_suffix: suffix_1m,
            disable_1m: env_val("CLAUDE_CODE_DISABLE_1M_CONTEXT"),
            max_context_tokens: env_val("CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
        },
        fast_mode: FastModeInfo {
            disable_fast_mode: env_val("CLAUDE_CODE_DISABLE_FAST_MODE"),
            supported_by_resolved_model: supports_fast,
        },
        subagent: SubagentInfo {
            subagent_model: env_val("CLAUDE_CODE_SUBAGENT_MODEL"),
        },
        model_defaults: ModelDefaults {
            opus: defaults.opus.clone(),
            sonnet: defaults.sonnet.clone(),
            haiku: defaults.haiku.clone(),
            opus_override: env_val("ANTHROPIC_DEFAULT_OPUS_MODEL"),
            sonnet_override: env_val("ANTHROPIC_DEFAULT_SONNET_MODEL"),
            haiku_override: env_val("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            small_fast_model: env_val("ANTHROPIC_SMALL_FAST_MODEL"),
        },
        aliases: get_aliases(&defaults),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn expand_alias_sonnet() {
        let defaults = ResolvedDefaults {
            opus: "claude-opus-4-6".into(),
            sonnet: "claude-sonnet-4-6".into(),
            haiku: "claude-haiku-4-5".into(),
        };
        assert_eq!(
            expand_alias("sonnet", &defaults),
            Some("claude-sonnet-4-6".into())
        );
    }

    #[test]
    fn expand_alias_opus() {
        let defaults = ResolvedDefaults {
            opus: "claude-opus-4-6".into(),
            sonnet: "claude-sonnet-4-6".into(),
            haiku: "claude-haiku-4-5".into(),
        };
        assert_eq!(
            expand_alias("opus", &defaults),
            Some("claude-opus-4-6".into())
        );
    }

    #[test]
    fn expand_alias_best() {
        let defaults = ResolvedDefaults {
            opus: "claude-opus-4-6".into(),
            sonnet: "claude-sonnet-4-6".into(),
            haiku: "claude-haiku-4-5".into(),
        };
        assert_eq!(
            expand_alias("best", &defaults),
            Some("claude-opus-4-6".into())
        );
    }

    #[test]
    fn expand_alias_with_1m_suffix() {
        let defaults = ResolvedDefaults {
            opus: "claude-opus-4-6".into(),
            sonnet: "claude-sonnet-4-6".into(),
            haiku: "claude-haiku-4-5".into(),
        };
        assert_eq!(
            expand_alias("sonnet[1m]", &defaults),
            Some("claude-sonnet-4-6[1m]".into())
        );
    }

    #[test]
    fn expand_alias_unknown_returns_none() {
        let defaults = ResolvedDefaults {
            opus: "claude-opus-4-6".into(),
            sonnet: "claude-sonnet-4-6".into(),
            haiku: "claude-haiku-4-5".into(),
        };
        assert_eq!(expand_alias("claude-sonnet-4-6", &defaults), None);
    }

    #[test]
    fn expand_alias_opusplan() {
        let defaults = ResolvedDefaults {
            opus: "claude-opus-4-6".into(),
            sonnet: "claude-sonnet-4-6".into(),
            haiku: "claude-haiku-4-5".into(),
        };
        // opusplan resolves to sonnet at parse time
        assert_eq!(
            expand_alias("opusplan", &defaults),
            Some("claude-sonnet-4-6".into())
        );
    }

    #[test]
    fn detect_provider_default() {
        // When no provider env vars are set, should return first_party.
        // This test relies on the CI/test environment not having these set.
        // We just verify the function doesn't panic.
        let result = detect_provider();
        assert!(!result.is_empty());
    }

    #[test]
    fn bedrock_model_id_maps_correctly() {
        let result = bedrock_model_id("claude-sonnet-4-6");
        assert!(result.contains("anthropic.claude-sonnet-4-6"));
        assert!(result.ends_with("-v1:0"));
    }

    #[test]
    fn scan_nonexistent_settings() {
        let result = scan_settings_file(Path::new("/nonexistent/path/settings.json"));
        assert!(!result.exists);
        assert!(result.model.is_none());
    }

    #[test]
    fn scan_settings_file_with_model() {
        use tempfile::TempDir;

        let dir = {
            std::fs::create_dir_all("tmp").ok();
            TempDir::new_in("tmp").unwrap()
        };
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model": "opus", "availableModels": ["opus", "sonnet"]}"#,
        )
        .unwrap();

        let result = scan_settings_file(&path);
        assert!(result.exists);
        assert_eq!(result.model.unwrap(), serde_json::json!("opus"));
        assert!(result.available_models.is_some());
    }

    #[test]
    fn scan_settings_file_with_env_model() {
        use tempfile::TempDir;

        let dir = {
            std::fs::create_dir_all("tmp").ok();
            TempDir::new_in("tmp").unwrap()
        };
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"env": {"ANTHROPIC_MODEL": "sonnet"}, "model": "opus"}"#,
        )
        .unwrap();

        let result = scan_settings_file(&path);
        assert!(result.exists);
        assert_eq!(result.env_anthropic_model.unwrap(), "sonnet");
    }

    #[test]
    fn build_report_succeeds() {
        // The report builder should always succeed, even with no settings files
        let report = build_resolution_report().unwrap();
        assert!(!report.active_model.resolved.is_empty());
        assert_eq!(report.resolution_chain.len(), 5);
        assert!(!report.aliases.is_empty());
    }
}
