//! Preflight validation checks for early failure detection
//!
//! This module provides functions to validate required services and credentials
//! before starting expensive operations. Commands should call these checks early
//! to fail fast with clear error messages.

use anyhow::{bail, Context, Result};

/// Result of AI credential validation
#[derive(Debug)]
pub struct AiCredentialInfo {
    /// The AI provider that will be used
    pub provider: AiProvider,
    /// The model that will be used
    pub model: String,
}

/// AI provider types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiProvider {
    /// Anthropic Claude API
    Claude,
    /// AWS Bedrock with Claude
    Bedrock,
    /// OpenAI API
    OpenAi,
    /// Local Ollama
    Ollama,
}

impl std::fmt::Display for AiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiProvider::Claude => write!(f, "Claude API"),
            AiProvider::Bedrock => write!(f, "AWS Bedrock"),
            AiProvider::OpenAi => write!(f, "OpenAI API"),
            AiProvider::Ollama => write!(f, "Ollama"),
        }
    }
}

/// Validate AI credentials are available before processing
///
/// This performs a lightweight check of environment variables without
/// creating a full AI client. Use this at the start of commands that
/// require AI to fail fast if credentials are missing.
pub fn check_ai_credentials(model_override: Option<&str>) -> Result<AiCredentialInfo> {
    use crate::utils::settings::{get_env_var, get_env_vars};

    // Check provider selection flags
    let use_openai = get_env_var("USE_OPENAI")
        .map(|val| val == "true")
        .unwrap_or(false);

    let use_ollama = get_env_var("USE_OLLAMA")
        .map(|val| val == "true")
        .unwrap_or(false);

    let use_bedrock = get_env_var("CLAUDE_CODE_USE_BEDROCK")
        .map(|val| val == "true")
        .unwrap_or(false);

    // Check Ollama (no credentials required, just model)
    if use_ollama {
        let model = model_override
            .map(String::from)
            .or_else(|| get_env_var("OLLAMA_MODEL").ok())
            .unwrap_or_else(|| "llama2".to_string());

        return Ok(AiCredentialInfo {
            provider: AiProvider::Ollama,
            model,
        });
    }

    // Check OpenAI
    if use_openai {
        let model = model_override
            .map(String::from)
            .or_else(|| get_env_var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| "gpt-5".to_string());

        // Verify API key exists
        get_env_vars(&["OPENAI_API_KEY", "OPENAI_AUTH_TOKEN"]).map_err(|_| {
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
        let model = model_override
            .map(String::from)
            .or_else(|| get_env_var("ANTHROPIC_MODEL").ok())
            .unwrap_or_else(|| "claude-opus-4-1-20250805".to_string());

        // Verify Bedrock configuration
        get_env_var("ANTHROPIC_AUTH_TOKEN").map_err(|_| {
            anyhow::anyhow!(
                "AWS Bedrock authentication not configured.\n\
                 Set ANTHROPIC_AUTH_TOKEN environment variable."
            )
        })?;

        get_env_var("ANTHROPIC_BEDROCK_BASE_URL").map_err(|_| {
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
    let model = model_override
        .map(String::from)
        .or_else(|| get_env_var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| "claude-opus-4-1-20250805".to_string());

    // Verify API key exists
    get_env_vars(&[
        "CLAUDE_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ])
    .map_err(|_| {
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

/// Validate GitHub CLI is available and authenticated
///
/// This checks:
/// 1. `gh` CLI is installed and in PATH
/// 2. User is authenticated (can access the current repo)
///
/// Use this at the start of commands that require GitHub API access.
pub fn check_github_cli() -> Result<()> {
    // Check if gh CLI is available
    let gh_check = std::process::Command::new("gh")
        .args(["--version"])
        .output();

    match gh_check {
        Ok(output) if output.status.success() => {
            // Test if gh can access the current repo
            let repo_check = std::process::Command::new("gh")
                .args(["repo", "view", "--json", "name"])
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
                    } else {
                        bail!(
                            "GitHub CLI cannot access this repository.\n\
                             Error: {}",
                            error_details.trim()
                        )
                    }
                }
                Err(e) => bail!("Failed to test GitHub CLI access: {}", e),
            }
        }
        _ => bail!(
            "GitHub CLI (gh) is not installed or not in PATH.\n\
             Please install it from https://cli.github.com/"
        ),
    }
}

/// Validate we're in a valid git repository
///
/// This is a lightweight check that opens the repository without
/// loading any commit data.
pub fn check_git_repository() -> Result<()> {
    crate::git::GitRepository::open().context(
        "Not in a git repository. Please run this command from within a git repository.",
    )?;
    Ok(())
}

/// Validate working directory is clean (no uncommitted changes)
///
/// This checks for:
/// - Staged changes
/// - Unstaged modifications
/// - Untracked files (excluding ignored files)
///
/// Use this before operations that require a clean working directory,
/// like amending commits.
pub fn check_working_directory_clean() -> Result<()> {
    let repo = crate::git::GitRepository::open().context("Failed to open git repository")?;

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

/// Combined preflight check for AI commands
///
/// Validates:
/// - Git repository access
/// - AI credentials
///
/// Returns information about the AI provider that will be used.
pub fn check_ai_command_prerequisites(model_override: Option<&str>) -> Result<AiCredentialInfo> {
    check_git_repository()?;
    check_ai_credentials(model_override)
}

/// Combined preflight check for PR creation
///
/// Validates:
/// - Git repository access
/// - AI credentials
/// - GitHub CLI availability and authentication
///
/// Returns information about the AI provider that will be used.
pub fn check_pr_command_prerequisites(model_override: Option<&str>) -> Result<AiCredentialInfo> {
    check_git_repository()?;
    let ai_info = check_ai_credentials(model_override)?;
    check_github_cli()?;
    Ok(ai_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ai_provider_display() {
        assert_eq!(format!("{}", AiProvider::Claude), "Claude API");
        assert_eq!(format!("{}", AiProvider::Bedrock), "AWS Bedrock");
        assert_eq!(format!("{}", AiProvider::OpenAi), "OpenAI API");
        assert_eq!(format!("{}", AiProvider::Ollama), "Ollama");
    }
}
