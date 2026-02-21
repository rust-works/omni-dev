//! Utility functions and helpers.

pub mod ai_scratch;
pub mod general;
pub mod preflight;
pub mod settings;

// Re-export commonly used items from general
pub use general::*;
pub use preflight::{
    check_ai_command_prerequisites, check_ai_credentials, check_git_repository, check_github_cli,
    check_pr_command_prerequisites, check_working_directory_clean, AiCredentialInfo, AiProvider,
};
pub use settings::{get_env_var, get_env_vars, Settings};
