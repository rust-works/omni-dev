//! Preflight validation checks for early failure detection.
//!
//! This module provides functions to validate required services and credentials
//! before starting expensive operations. Commands should call these checks early
//! to fail fast with clear error messages.

use anyhow::{bail, Context, Result};

use crate::claude::model_config::get_model_registry;

/// Result of AI credential validation.
#[derive(Debug)]
pub struct AiCredentialInfo {
    /// The AI provider that will be used.
    pub provider: AiProvider,
    /// The model that will be used.
    pub model: String,
}

/// AI provider types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiProvider {
    /// Anthropic Claude API.
    Claude,
    /// AWS Bedrock with Claude.
    Bedrock,
    /// OpenAI API.
    OpenAi,
    /// Local Ollama.
    Ollama,
    /// `claude -p` subprocess (Claude Code CLI).
    ClaudeCli,
}

impl std::fmt::Display for AiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claude => write!(f, "Claude API"),
            Self::Bedrock => write!(f, "AWS Bedrock"),
            Self::OpenAi => write!(f, "OpenAI API"),
            Self::Ollama => write!(f, "Ollama"),
            Self::ClaudeCli => write!(f, "Claude Code CLI"),
        }
    }
}

/// Validates that AI credentials are available before processing.
///
/// This performs a lightweight check of environment variables without
/// creating a full AI client. Use this at the start of commands that
/// require AI to fail fast if credentials are missing.
pub fn check_ai_credentials(model_override: Option<&str>) -> Result<AiCredentialInfo> {
    check_ai_credentials_with(&crate::utils::settings::SettingsEnv::load(), model_override)
}

/// [`check_ai_credentials`] over an injected
/// [`EnvSource`](crate::utils::env::EnvSource).
///
/// The production wrapper passes `&SettingsEnv::load()` (process env with a
/// settings.json fallback); tests pass a pure `MapEnv`, so this env-parsing
/// boundary is exercised without mutating the process environment or taking a
/// lock (issue #1030).
pub(crate) fn check_ai_credentials_with(
    env: &impl crate::utils::env::EnvSource,
    model_override: Option<&str>,
) -> Result<AiCredentialInfo> {
    // The `claude -p` subprocess backend is checked first so it wins over
    // the existing USE_* flags if multiple are set. Credentials for this
    // backend live inside the `claude` binary's own auth state, so we just
    // verify the binary is on PATH.
    if let Some(val) = env.var("OMNI_DEV_AI_BACKEND") {
        if matches!(val.as_str(), "claude-cli" | "claude_cli") {
            let binary = env
                .var("OMNI_DEV_CLAUDE_CLI_BIN")
                .unwrap_or_else(|| "claude".to_string());
            let probe = std::process::Command::new(&binary)
                .arg("--version")
                .output();
            match probe {
                Ok(out) if out.status.success() => {
                    let registry = get_model_registry();
                    let model = model_override
                        .map(String::from)
                        .or_else(|| env.var("CLAUDE_MODEL"))
                        .or_else(|| env.var("CLAUDE_CODE_MODEL"))
                        .or_else(|| env.var("ANTHROPIC_MODEL"))
                        .unwrap_or_else(|| {
                            registry
                                .get_default_model("claude")
                                .unwrap_or("claude-sonnet-4-6")
                                .to_string()
                        });
                    return Ok(AiCredentialInfo {
                        provider: AiProvider::ClaudeCli,
                        model,
                    });
                }
                _ => bail!(
                    "Claude Code CLI not available at '{binary}'.\n\
                     Install it from https://github.com/anthropics/claude-code \
                     or set OMNI_DEV_CLAUDE_CLI_BIN to its path."
                ),
            }
        }
    }

    // Check provider selection flags
    let use_openai = env.var("USE_OPENAI").is_some_and(|val| val == "true");

    let use_ollama = env.var("USE_OLLAMA").is_some_and(|val| val == "true");

    let use_bedrock = env
        .var("CLAUDE_CODE_USE_BEDROCK")
        .is_some_and(|val| val == "true");

    // Check Ollama (no credentials required, just model)
    if use_ollama {
        let model = model_override
            .map(String::from)
            .or_else(|| env.var("OLLAMA_MODEL"))
            .unwrap_or_else(|| "llama2".to_string());

        return Ok(AiCredentialInfo {
            provider: AiProvider::Ollama,
            model,
        });
    }

    // Check OpenAI
    if use_openai {
        let registry = get_model_registry();
        let model = model_override
            .map(String::from)
            .or_else(|| env.var("OPENAI_MODEL"))
            .unwrap_or_else(|| {
                registry
                    .get_default_model("openai")
                    .unwrap_or("gpt-5")
                    .to_string()
            });

        // Verify API key exists
        env.var_any(&["OPENAI_API_KEY", "OPENAI_AUTH_TOKEN"])
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "OpenAI API key not found.\n\
                 Set one of these environment variables:\n\
                 - OPENAI_API_KEY\n\
                 - OPENAI_AUTH_TOKEN"
                )
            })?;

        return Ok(AiCredentialInfo {
            provider: AiProvider::OpenAi,
            model,
        });
    }

    // Check Bedrock
    if use_bedrock {
        let registry = get_model_registry();
        let model = model_override
            .map(String::from)
            .or_else(|| env.var("ANTHROPIC_MODEL"))
            .unwrap_or_else(|| {
                registry
                    .get_default_model("claude")
                    .unwrap_or("claude-sonnet-4-6")
                    .to_string()
            });

        // Verify Bedrock configuration
        env.var("ANTHROPIC_AUTH_TOKEN").ok_or_else(|| {
            anyhow::anyhow!(
                "AWS Bedrock authentication not configured.\n\
                 Set ANTHROPIC_AUTH_TOKEN environment variable."
            )
        })?;

        env.var("ANTHROPIC_BEDROCK_BASE_URL").ok_or_else(|| {
            anyhow::anyhow!(
                "AWS Bedrock base URL not configured.\n\
                 Set ANTHROPIC_BEDROCK_BASE_URL environment variable."
            )
        })?;

        return Ok(AiCredentialInfo {
            provider: AiProvider::Bedrock,
            model,
        });
    }

    // Default: Claude API
    let registry = get_model_registry();
    let model = model_override
        .map(String::from)
        .or_else(|| env.var("ANTHROPIC_MODEL"))
        .unwrap_or_else(|| {
            registry
                .get_default_model("claude")
                .unwrap_or("claude-sonnet-4-6")
                .to_string()
        });

    // Verify API key exists
    env.var_any(&[
        "CLAUDE_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ])
    .ok_or_else(|| {
        anyhow::anyhow!(
            "Claude API key not found.\n\
                 Set one of these environment variables:\n\
                 - CLAUDE_API_KEY\n\
                 - ANTHROPIC_API_KEY\n\
                 - ANTHROPIC_AUTH_TOKEN"
        )
    })?;

    Ok(AiCredentialInfo {
        provider: AiProvider::Claude,
        model,
    })
}

/// Validates that GitHub CLI is available and authenticated.
///
/// This checks:
/// 1. `gh` CLI is installed and in PATH
/// 2. User is authenticated (can access the current repo)
///
/// Use this at the start of commands that require GitHub API access.
///
/// `repo_root` anchors the repository-access probe to the injected repository
/// rather than the process current working directory.
pub fn check_github_cli(repo_root: &std::path::Path) -> Result<()> {
    // Check if gh CLI is available. This probe is a PATH availability check
    // (CWD-independent), so it is not anchored to `repo_root`.
    let gh_check = std::process::Command::new("gh")
        .args(["--version"])
        .output();

    match gh_check {
        Ok(output) if output.status.success() => {
            // Test if gh can access the injected repo
            let repo_check = std::process::Command::new("gh")
                .args(["repo", "view", "--json", "name"])
                .current_dir(repo_root)
                .output();

            match repo_check {
                Ok(repo_output) if repo_output.status.success() => Ok(()),
                Ok(repo_output) => {
                    let error_details = String::from_utf8_lossy(&repo_output.stderr);
                    if error_details.contains("authentication") || error_details.contains("login") {
                        bail!(
                            "GitHub CLI authentication failed.\n\
                             Please run 'gh auth login' or set GITHUB_TOKEN environment variable."
                        )
                    }
                    bail!(
                        "GitHub CLI cannot access this repository.\n\
                         Error: {}",
                        error_details.trim()
                    )
                }
                Err(e) => bail!("Failed to test GitHub CLI access: {e}"),
            }
        }
        _ => bail!(
            "GitHub CLI (gh) is not installed or not in PATH.\n\
             Please install it from https://cli.github.com/"
        ),
    }
}

/// Validates that `repo_root` is a valid git repository.
///
/// A lightweight check that opens the repository without loading commit data.
pub fn check_git_repository_at(repo_root: &std::path::Path) -> Result<()> {
    crate::git::GitRepository::open_at(repo_root).context(
        "Not in a git repository. Please run this command from within a git repository.",
    )?;
    Ok(())
}

/// Validates that the working directory at `repo_root` is clean — no
/// uncommitted changes (staged, unstaged, or untracked non-ignored files).
///
/// Use this before operations that require a clean working directory, like
/// amending commits.
pub fn check_working_directory_clean_at(repo_root: &std::path::Path) -> Result<()> {
    let repo =
        crate::git::GitRepository::open_at(repo_root).context("Failed to open git repository")?;
    check_working_directory_clean_for(&repo)
}

/// Shared clean-worktree check over an already-opened repository.
fn check_working_directory_clean_for(repo: &crate::git::GitRepository) -> Result<()> {
    let status = repo
        .get_working_directory_status()
        .context("Failed to get working directory status")?;

    if !status.clean {
        let mut message = String::from("Working directory has uncommitted changes:\n");
        for change in &status.untracked_changes {
            message.push_str(&format!("  {} {}\n", change.status, change.file));
        }
        message.push_str("\nPlease commit or stash your changes before proceeding.");
        bail!(message);
    }

    Ok(())
}

/// Performs combined preflight check for AI commands.
///
/// Validates:
/// - Git repository access
/// - AI credentials
///
/// Returns information about the AI provider that will be used.
///
/// `repo_root` anchors the git-repository check to the injected repository
/// rather than the process current working directory.
pub fn check_ai_command_prerequisites(
    model_override: Option<&str>,
    repo_root: &std::path::Path,
) -> Result<AiCredentialInfo> {
    check_git_repository_at(repo_root)?;
    check_ai_credentials(model_override)
}

/// Performs combined preflight check for PR creation.
///
/// Validates:
/// - Git repository access
/// - AI credentials
/// - GitHub CLI availability and authentication
///
/// Returns information about the AI provider that will be used.
///
/// `repo_root` anchors the git-repository and GitHub CLI checks to the injected
/// repository rather than the process current working directory.
pub fn check_pr_command_prerequisites(
    model_override: Option<&str>,
    repo_root: &std::path::Path,
) -> Result<AiCredentialInfo> {
    check_git_repository_at(repo_root)?;
    let ai_info = check_ai_credentials(model_override)?;
    check_github_cli(repo_root)?;
    Ok(ai_info)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    #[test]
    fn ai_provider_display() {
        assert_eq!(format!("{}", AiProvider::Claude), "Claude API");
        assert_eq!(format!("{}", AiProvider::Bedrock), "AWS Bedrock");
        assert_eq!(format!("{}", AiProvider::OpenAi), "OpenAI API");
        assert_eq!(format!("{}", AiProvider::Ollama), "Ollama");
        assert_eq!(format!("{}", AiProvider::ClaudeCli), "Claude Code CLI");
    }

    #[test]
    fn ai_provider_equality() {
        assert_eq!(AiProvider::Claude, AiProvider::Claude);
        assert_ne!(AiProvider::Claude, AiProvider::OpenAi);
        assert_ne!(AiProvider::Bedrock, AiProvider::Ollama);
    }

    #[test]
    fn ai_provider_clone() {
        let provider = AiProvider::Bedrock;
        let cloned = provider;
        assert_eq!(provider, cloned);
    }

    #[test]
    fn ai_provider_debug() {
        let debug_str = format!("{:?}", AiProvider::Claude);
        assert_eq!(debug_str, "Claude");
    }

    #[test]
    fn ai_credential_info_debug() {
        let info = AiCredentialInfo {
            provider: AiProvider::Ollama,
            model: "llama2".to_string(),
        };
        let debug_str = format!("{info:?}");
        assert!(debug_str.contains("Ollama"));
        assert!(debug_str.contains("llama2"));
    }

    #[test]
    fn claude_default_model_from_registry() {
        // Claude API path with a dummy key, no model override. A pure MapEnv
        // means absent vars (USE_OPENAI, …) simply read as None — no need to
        // clear anything, and no process-global env is touched.
        let env = MapEnv::new().with("ANTHROPIC_API_KEY", "sk-test-dummy");

        let info = check_ai_credentials_with(&env, None).unwrap();
        assert_eq!(info.provider, AiProvider::Claude);
        assert_eq!(info.model, "claude-sonnet-4-6");
    }

    #[test]
    fn openai_default_model_from_registry() {
        let env = MapEnv::new()
            .with("USE_OPENAI", "true")
            .with("OPENAI_API_KEY", "sk-test-dummy");

        let info = check_ai_credentials_with(&env, None).unwrap();
        assert_eq!(info.provider, AiProvider::OpenAi);
        assert_eq!(info.model, "gpt-5-mini");
    }

    #[test]
    fn bedrock_default_model_from_registry() {
        let env = MapEnv::new()
            .with("CLAUDE_CODE_USE_BEDROCK", "true")
            .with("ANTHROPIC_AUTH_TOKEN", "test-token")
            .with("ANTHROPIC_BEDROCK_BASE_URL", "https://bedrock.example.com");

        let info = check_ai_credentials_with(&env, None).unwrap();
        assert_eq!(info.provider, AiProvider::Bedrock);
        assert_eq!(info.model, "claude-sonnet-4-6");
    }

    #[test]
    fn model_override_takes_precedence() {
        let env = MapEnv::new().with("ANTHROPIC_API_KEY", "sk-test-dummy");

        let info = check_ai_credentials_with(&env, Some("claude-opus-4-6")).unwrap();
        assert_eq!(info.model, "claude-opus-4-6");
    }

    #[cfg(unix)]
    fn make_version_shim(tmp: &tempfile::TempDir, exit_code: i32) -> std::path::PathBuf {
        let shim = tmp.path().join("claude-bin-shim");
        crate::test_support::shim::write_exec_script(
            &shim,
            &format!("#!/bin/sh\necho 'fake-claude 0.0.0'\nexit {exit_code}\n"),
        );
        shim
    }

    #[test]
    #[cfg(unix)]
    fn claude_cli_backend_uses_version_probe() {
        // shim_lock guards the exec-script/ETXTBSY race (#642), not env.
        let _guard = crate::test_support::shim::shim_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let shim = make_version_shim(&tmp, 0);

        let env = MapEnv::new()
            .with("OMNI_DEV_AI_BACKEND", "claude-cli")
            .with("OMNI_DEV_CLAUDE_CLI_BIN", shim.to_str().unwrap());

        let info = check_ai_credentials_with(&env, None).unwrap();
        assert_eq!(info.provider, AiProvider::ClaudeCli);
        assert_eq!(info.model, "claude-sonnet-4-6");
    }

    #[test]
    #[cfg(unix)]
    fn claude_cli_backend_uses_model_from_env() {
        // shim_lock guards the exec-script/ETXTBSY race (#642), not env.
        let _guard = crate::test_support::shim::shim_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let shim = make_version_shim(&tmp, 0);

        let env = MapEnv::new()
            .with("OMNI_DEV_AI_BACKEND", "claude-cli")
            .with("OMNI_DEV_CLAUDE_CLI_BIN", shim.to_str().unwrap())
            .with("CLAUDE_MODEL", "haiku");

        let info = check_ai_credentials_with(&env, None).unwrap();
        assert_eq!(info.provider, AiProvider::ClaudeCli);
        assert_eq!(info.model, "haiku");
    }

    #[test]
    fn claude_cli_backend_missing_binary_fails_preflight() {
        // A nonexistent binary path never spawns, so no shim_lock is needed.
        let env = MapEnv::new()
            .with("OMNI_DEV_AI_BACKEND", "claude-cli")
            .with("OMNI_DEV_CLAUDE_CLI_BIN", "/nonexistent/claude-binary-xyz");

        let err = check_ai_credentials_with(&env, None).expect_err("expected missing-binary error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Claude Code CLI not available"),
            "unexpected error: {chain}"
        );
    }

    #[test]
    fn claude_cli_backend_accepts_underscore_alias() {
        // The factory/preflight accept both `claude-cli` and `claude_cli`.
        // Verify the second spelling routes the same way (missing-binary
        // path exercises the selector cheaply).
        let env = MapEnv::new()
            .with("OMNI_DEV_AI_BACKEND", "claude_cli")
            .with("OMNI_DEV_CLAUDE_CLI_BIN", "/nonexistent/claude-binary-xyz");

        let err = check_ai_credentials_with(&env, None).expect_err("expected missing-binary error");
        let chain = format!("{err:#}");
        assert!(chain.contains("Claude Code CLI not available"));
    }
}
