//! Git-related CLI commands.

mod amend;
mod check;
mod create_pr;
pub(crate) mod formatting;
mod info;
mod staged;
mod twiddle;
mod view;

pub use amend::AmendCommand;
pub use check::{run_check, CheckCommand, CheckOutcome};
pub use create_pr::{run_create_pr, CreatePrCommand, CreatePrOutcome, PrContent};
pub use info::{run_info, InfoCommand};
pub use staged::{run_staged, StagedCommand, StagedOutcome};
pub use twiddle::{run_twiddle, TwiddleCommand, TwiddleOutcome};
pub use view::{run_view, ViewCommand};

use std::path::Path;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Global async mutex serialising every caller that mutates the process-wide
/// current working directory via `std::env::set_current_dir`.
///
/// Used by:
/// - The production [`CwdGuard`] wrapper that MCP tool handlers acquire via
///   `.lock().await`.
/// - Async unit tests that call `CwdGuard::enter` directly (e.g., `check`,
///   `twiddle`, `create_pr`).
/// - Sync unit tests that change CWD directly; they acquire the same mutex
///   via [`tokio::sync::Mutex::blocking_lock`] so both styles of test
///   serialise through one instance and cannot race on the shared CWD.
///
/// We use `tokio::sync::Mutex` rather than `std::sync::Mutex` so the guard is
/// `Send` and can be held across `.await` points (required by the MCP
/// async tool handlers).
pub(crate) static CWD_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII guard that temporarily changes the process current working directory
/// and restores it on drop.
///
/// Shared by MCP tool handlers that accept a `repo_path` parameter: many
/// commands (check/twiddle/create_pr) read configuration and invoke external
/// tools relative to the current working directory, so the simplest way to
/// "run this command at a different path" is to pin the CWD for the duration
/// of the call. A global async mutex serialises concurrent callers.
pub(crate) struct CwdGuard {
    original: std::path::PathBuf,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl CwdGuard {
    /// Enters `path`, holding the CWD mutex for the lifetime of the guard.
    pub(crate) async fn enter<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let lock = CWD_MUTEX.lock().await;
        let original =
            std::env::current_dir().map_err(|e| anyhow::anyhow!("current_dir failed: {e}"))?;
        std::env::set_current_dir(path.as_ref())
            .map_err(|e| anyhow::anyhow!("set_current_dir failed: {e}"))?;
        Ok(Self {
            original,
            _lock: lock,
        })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Reads one line of interactive input from `reader`.
///
/// Returns `Some(line)` on success, or `None` when the reader reaches EOF
/// (i.e., `read_line` returns 0 bytes). Callers handle the `None` case
/// with context-specific warnings and control flow.
pub(super) fn read_interactive_line(
    reader: &mut (dyn std::io::BufRead + Send),
) -> std::io::Result<Option<String>> {
    let mut input = String::new();
    let bytes = reader.read_line(&mut input)?;
    if bytes == 0 {
        Ok(None)
    } else {
        Ok(Some(input))
    }
}

/// Parses a `--beta-header key:value` string into a `(key, value)` tuple.
pub(crate) fn parse_beta_header(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid --beta-header format '{s}'. Expected key:value"))?;
    Ok((k.to_string(), v.to_string()))
}

/// Git operations.
#[derive(Parser)]
pub struct GitCommand {
    /// Git subcommand to execute.
    #[command(subcommand)]
    pub command: GitSubcommands,
}

/// Git subcommands.
#[derive(Subcommand)]
pub enum GitSubcommands {
    /// Commit-related operations.
    Commit(CommitCommand),
    /// Branch-related operations.
    Branch(BranchCommand),
}

/// Commit operations.
#[derive(Parser)]
pub struct CommitCommand {
    /// Commit subcommand to execute.
    #[command(subcommand)]
    pub command: CommitSubcommands,
}

/// Commit subcommands.
#[derive(Subcommand)]
pub enum CommitSubcommands {
    /// Commit message operations.
    Message(MessageCommand),
}

/// Message operations.
#[derive(Parser)]
pub struct MessageCommand {
    /// Message subcommand to execute.
    #[command(subcommand)]
    pub command: MessageSubcommands,
}

/// Message subcommands.
#[derive(Subcommand)]
pub enum MessageSubcommands {
    /// Analyzes commits and outputs repository information in YAML format.
    View(ViewCommand),
    /// Amends commit messages based on a YAML configuration file.
    Amend(AmendCommand),
    /// AI-powered commit message improvement using Claude.
    Twiddle(TwiddleCommand),
    /// Checks commit messages against guidelines without modifying them.
    Check(CheckCommand),
    /// Generates a commit message from staged changes and commits them.
    Staged(StagedCommand),
}

/// Branch operations.
#[derive(Parser)]
pub struct BranchCommand {
    /// Branch subcommand to execute.
    #[command(subcommand)]
    pub command: BranchSubcommands,
}

/// Branch subcommands.
#[derive(Subcommand)]
pub enum BranchSubcommands {
    /// Analyzes branch commits and outputs repository information in YAML format.
    Info(InfoCommand),
    /// Create operations.
    Create(CreateCommand),
}

/// Create operations.
#[derive(Parser)]
pub struct CreateCommand {
    /// Create subcommand to execute.
    #[command(subcommand)]
    pub command: CreateSubcommands,
}

/// Create subcommands.
#[derive(Subcommand)]
pub enum CreateSubcommands {
    /// Creates a pull request with AI-generated description.
    Pr(CreatePrCommand),
}

impl GitCommand {
    /// Executes the git command.
    ///
    /// `repo` is the repository location resolved once at the CLI boundary
    /// (`None` = current working directory); it is threaded explicitly down to
    /// each leaf command rather than read from the ambient CWD.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        match self.command {
            GitSubcommands::Commit(commit_cmd) => commit_cmd.execute(repo).await,
            GitSubcommands::Branch(branch_cmd) => branch_cmd.execute(repo).await,
        }
    }
}

impl CommitCommand {
    /// Executes the commit command.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        match self.command {
            CommitSubcommands::Message(message_cmd) => message_cmd.execute(repo).await,
        }
    }
}

impl MessageCommand {
    /// Executes the message command.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        match self.command {
            MessageSubcommands::View(view_cmd) => view_cmd.execute(repo),
            MessageSubcommands::Amend(amend_cmd) => amend_cmd.execute(repo),
            MessageSubcommands::Twiddle(twiddle_cmd) => twiddle_cmd.execute(repo).await,
            MessageSubcommands::Check(check_cmd) => check_cmd.execute(repo).await,
            MessageSubcommands::Staged(staged_cmd) => staged_cmd.execute(repo).await,
        }
    }
}

impl BranchCommand {
    /// Executes the branch command.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        match self.command {
            BranchSubcommands::Info(info_cmd) => info_cmd.execute(repo),
            BranchSubcommands::Create(create_cmd) => create_cmd.execute(repo).await,
        }
    }
}

impl CreateCommand {
    /// Executes the create command.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        match self.command {
            CreateSubcommands::Pr(pr_cmd) => pr_cmd.execute(repo).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    // Parser trait must be in scope for try_parse_from
    use clap::Parser as _ClapParser;

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

    #[test]
    fn cli_parses_git_commit_message_view() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "view",
            "HEAD~3..HEAD",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_commit_message_amend() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "amend",
            "amendments.yaml",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_branch_info() {
        let cli = Cli::try_parse_from(["omni-dev", "git", "branch", "info"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_branch_info_with_base() {
        let cli = Cli::try_parse_from(["omni-dev", "git", "branch", "info", "develop"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_config_models_show() {
        let cli = Cli::try_parse_from(["omni-dev", "config", "models", "show"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_help_all() {
        let cli = Cli::try_parse_from(["omni-dev", "help-all"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_rejects_unknown_command() {
        let cli = Cli::try_parse_from(["omni-dev", "nonexistent"]);
        assert!(cli.is_err());
    }

    #[test]
    fn cli_parses_twiddle_with_options() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "twiddle",
            "--auto-apply",
            "--no-context",
            "--concurrency",
            "8",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_check_with_options() {
        let cli = Cli::try_parse_from([
            "omni-dev", "git", "commit", "message", "check", "--strict", "--quiet", "--format",
            "json",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_commit_message_staged() {
        let cli = Cli::try_parse_from(["omni-dev", "git", "commit", "message", "staged"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_commit_message_staged_print_only() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "staged",
            "--print-only",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_git_commit_message_staged_with_model_and_beta() {
        let cli = Cli::try_parse_from([
            "omni-dev",
            "git",
            "commit",
            "message",
            "staged",
            "--model",
            "claude-sonnet-4-6",
            "--beta-header",
            "anthropic-beta:output-128k-2025-02-19",
        ]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_commands_generate_all() {
        let cli = Cli::try_parse_from(["omni-dev", "commands", "generate", "all"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_ai_chat() {
        let cli = Cli::try_parse_from(["omni-dev", "ai", "chat"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_ai_chat_with_model() {
        let cli = Cli::try_parse_from(["omni-dev", "ai", "chat", "--model", "claude-sonnet-4"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn cli_parses_ai_claude_cli_model_resolve() {
        let cli = Cli::try_parse_from(["omni-dev", "ai", "claude", "cli", "model", "resolve"]);
        assert!(cli.is_ok(), "Failed to parse: {:?}", cli.err());
    }

    #[test]
    fn read_interactive_line_returns_input() {
        let mut reader = std::io::Cursor::new(b"hello\n" as &[u8]);
        let result = read_interactive_line(&mut reader).unwrap();
        assert_eq!(result, Some("hello\n".to_string()));
    }

    #[test]
    fn read_interactive_line_eof_returns_none() {
        let mut reader = std::io::Cursor::new(b"" as &[u8]);
        let result = read_interactive_line(&mut reader).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn read_interactive_line_empty_line() {
        let mut reader = std::io::Cursor::new(b"\n" as &[u8]);
        let result = read_interactive_line(&mut reader).unwrap();
        assert_eq!(result, Some("\n".to_string()));
    }

    #[tokio::test]
    async fn cwd_guard_invalid_path_returns_error() {
        // Error path doesn't mutate the shared CWD, so it is safe to run in
        // parallel with the rest of the test suite. The happy path is covered
        // indirectly by `run_{check,twiddle,create_pr}` error-path tests
        // that exercise `CwdGuard::enter(valid_path)` followed by restoration.
        let result = CwdGuard::enter("/no/such/path/exists").await;
        assert!(result.is_err(), "expected error for nonexistent path");
    }

    /// Every git command that has not yet been converted to honor `--repo`
    /// must reject an injected path with a clear error rather than silently
    /// ignoring it (RULE 6). Exercises each reject-guard branch through the
    /// real parse + dispatch path. `git branch info`, `git commit message
    /// view`, and `git commit message staged` are converted, so they are
    /// absent here.
    #[tokio::test]
    async fn repo_flag_rejected_for_unconverted_commands() {
        let unconverted: [&[&str]; 4] = [
            &[
                "omni-dev", "-C", "/tmp", "git", "commit", "message", "amend", "x.yaml",
            ],
            &[
                "omni-dev", "-C", "/tmp", "git", "commit", "message", "twiddle",
            ],
            &[
                "omni-dev", "-C", "/tmp", "git", "commit", "message", "check",
            ],
            &["omni-dev", "-C", "/tmp", "git", "branch", "create", "pr"],
        ];
        for args in unconverted {
            let cli = Cli::try_parse_from(args.iter().copied()).unwrap();
            let err = cli
                .execute()
                .await
                .expect_err("unconverted command must reject --repo");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("not yet supported"),
                "args {args:?} -> unexpected error: {msg}"
            );
        }
    }
}
