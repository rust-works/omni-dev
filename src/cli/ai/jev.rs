//! `omni-dev ai jev` — TypeSafe Jev "System One" typed judgments.
//!
//! Wraps [`crate::jev::client::JevClient`]'s single `system_one` call behind
//! four subcommands: `choice`, `score`, `noul` (single-question) and `ask`
//! (multi-question, from a file), plus `route`, which asks the stage questions
//! of [`crate::jev::route`] about GitHub issues, `exists`, which retrieves local
//! signatures for existence screening, and `verify-decision`, which
//! checks a decision comment against the sources it cites
//! ([`crate::jev::verify`]). See `docs/jev.md` for the operator guide.

mod ask;
mod choice;
mod common;
mod exists;
mod noul;
mod route;
mod score;
mod verify_decision;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// TypeSafe Jev typed judgments (choice, score, yes/no).
#[derive(Parser)]
#[command(
    long_about = "TypeSafe Jev typed judgments (choice, score, yes/no).\n\nJev is a \
separate typed-judgment API, not a chat model, so most jev subcommands do **not** accept \
the AI backend flags (`--ai-backend`, `--model`, `--claude-cli-*`, ...) — passing any \
of them is a clap error on those leaves. `route`, `exists` and `verify-decision` are the three \
exceptions to `-C/--repo`, and `verify-decision` is the one subcommand that also accepts \
the AI backend flags, since it uses an AI backend to split a decision comment into \
statements before checking them with Jev. An exported `OMNI_DEV_MODEL` is silently ignored \
by every Jev call. Use `--jev-model` on each subcommand instead to override the Jev model."
)]
pub struct JevCommand {
    /// The jev subcommand to execute.
    #[command(subcommand)]
    pub command: JevSubcommands,
}

/// Jev subcommands.
#[derive(Subcommand)]
pub enum JevSubcommands {
    /// Picks one of a labelled set of options.
    Choice(choice::ChoiceCommand),
    /// Rates the state against an ordered scale.
    Score(score::ScoreCommand),
    /// Estimates the probability of a yes/no condition.
    Noul(noul::NoulCommand),
    /// Answers several questions about one state in a single pass, from a file.
    Ask(ask::AskCommand),
    /// Routes issues to model classes for their design, implement and review stages.
    Route(route::RouteCommand),
    /// Screens local Rust signatures for work an issue proposes writing.
    Exists(exists::ExistsCommand),
    /// Checks a decision comment against the issues or pull requests it cites.
    VerifyDecision(verify_decision::VerifyDecisionCommand),
}

impl JevCommand {
    /// Executes the jev command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            JevSubcommands::Choice(cmd) => cmd.execute().await,
            JevSubcommands::Score(cmd) => cmd.execute().await,
            JevSubcommands::Noul(cmd) => cmd.execute().await,
            JevSubcommands::Ask(cmd) => cmd.execute().await,
            JevSubcommands::Route(cmd) => cmd.execute().await,
            JevSubcommands::Exists(cmd) => cmd.execute().await,
            JevSubcommands::VerifyDecision(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(feature = "mcp")]
pub(crate) use common::{format_output_with_cache, JevFormat};
#[cfg(feature = "mcp")]
pub(crate) use route::{build_ladders, fetch_docs};
#[cfg(feature = "mcp")]
pub(crate) use verify_decision::fetch_input;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exists_runs_through_the_group_dispatch() {
        // A dry run needs neither credentials nor a network, so the real
        // `execute` shell (process environment, stdout) is safe to drive.
        let dir = crate::test_support::git_repo::commit_files(&[("walk.rs", b"fn caller() {}\n")]);
        let issue = dir.path().join("issue.txt");
        std::fs::write(&issue, "Reuse `caller`.").unwrap();
        let command = JevCommand::try_parse_from([
            "jev",
            "exists",
            "--issue-file",
            issue.to_str().unwrap(),
            "-C",
            dir.path().to_str().unwrap(),
            "--dry-run",
        ])
        .unwrap();
        command.execute().await.unwrap();
    }
}
