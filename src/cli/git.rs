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

/// Computes the default commit range when the user gave none:
/// `<base>..HEAD` with the base resolved remote-first (see
/// [`crate::git::GitRepository::resolve_default_base_branch`]).
pub(crate) fn default_commit_range(repo: &crate::git::GitRepository) -> Result<String> {
    match repo.resolve_default_base_branch() {
        Some(base) => Ok(format!("{base}..HEAD")),
        None => anyhow::bail!(
            "No default base branch found (checked origin/main, origin/master, main, master). \
             Pass an explicit commit range (e.g. 'origin/develop..HEAD') or base branch."
        ),
    }
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
    /// Analyzes commits and outputs repository information in YAML format (mirrors the `git_view_commits` MCP tool).
    View(ViewCommand),
    /// Amends commit messages based on a YAML configuration file.
    Amend(AmendCommand),
    /// AI-powered commit message improvement using Claude (mirrors the `git_twiddle_commits` MCP tool).
    Twiddle(TwiddleCommand),
    /// Checks commit messages against guidelines without modifying them (mirrors the `git_check_commits` MCP tool).
    Check(CheckCommand),
    /// Generates a commit message from staged changes and commits them (mirrors the `git_staged_commit` MCP tool).
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
    /// Analyzes branch commits and outputs repository information in YAML format (mirrors the `git_branch_info` MCP tool).
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
    /// Creates a pull request with AI-generated description (mirrors the `git_create_pr` MCP tool).
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

    /// Creates a temp repo with one commit on `branch`, anchored at
    /// `$CARGO_MANIFEST_DIR/tmp` like the other git test fixtures.
    fn repo_on_branch(branch: &str) -> (tempfile::TempDir, crate::git::GitRepository) {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let p = temp_dir.path();
        for args in [
            vec!["init"],
            vec!["checkout", "-b", branch],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            let output = std::process::Command::new("git")
                .current_dir(p)
                .args([
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "user.name=Test",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(&args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let repo = crate::git::GitRepository::open_at(p).unwrap();
        (temp_dir, repo)
    }

    #[test]
    fn default_commit_range_uses_resolved_base() {
        let (_tmp, repo) = repo_on_branch("main");
        assert_eq!(default_commit_range(&repo).unwrap(), "main..HEAD");
    }

    #[test]
    fn default_commit_range_errors_without_mainline() {
        let (_tmp, repo) = repo_on_branch("dev");
        let err = default_commit_range(&repo).unwrap_err().to_string();
        assert!(
            err.contains("No default base branch found") && err.contains("origin/main"),
            "unexpected error: {err}"
        );
    }

    /// All `git` message and branch commands now honor `--repo` by threading
    /// an explicit repo root through their reads (RULE 6 fully satisfied):
    /// `git branch info`, `git branch create pr`, `git commit message view`,
    /// `git commit message staged`, `git commit message check`, `git commit
    /// message amend`, and `git commit message twiddle` are all converted, so
    /// there are no remaining reject-guards. The empty array keeps this guard
    /// in place: should a future unconverted command be added, list it here so
    /// it is asserted to reject `--repo` rather than silently ignoring it.
    #[tokio::test]
    async fn repo_flag_rejected_for_unconverted_commands() {
        let unconverted: [&[&str]; 0] = [];
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
