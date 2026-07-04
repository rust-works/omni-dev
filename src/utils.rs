//! Utility functions and helpers.

pub mod ai_scratch;
pub mod env;
pub(crate) mod http;
pub(crate) mod path;
pub mod preflight;
pub mod secret;
pub mod settings;

pub use env::{EnvSource, SystemEnv};

pub use preflight::{
    check_ai_command_prerequisites, check_ai_credentials, check_git_repository_at,
    check_github_cli, check_pr_command_prerequisites, check_working_directory_clean_at,
    AiCredentialInfo, AiProvider,
};
pub use secret::Secret;
pub use settings::{get_env_var, get_env_vars, Settings};
