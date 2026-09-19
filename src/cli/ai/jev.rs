//! `omni-dev ai jev` — TypeSafe Jev "System One" typed judgments.
//!
//! Wraps [`crate::jev::client::JevClient`]'s single `system_one` call behind
//! four subcommands: `choice`, `score`, `noul` (single-question) and `ask`
//! (multi-question, from a file). See `docs/jev.md` for the operator guide.

mod ask;
mod choice;
mod common;
mod noul;
mod score;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// TypeSafe Jev typed judgments (choice, score, yes/no).
#[derive(Parser)]
#[command(
    long_about = "TypeSafe Jev typed judgments (choice, score, yes/no).\n\nJev is a \
separate typed-judgment API, not a chat model, so jev subcommands do **not** accept \
the AI backend flags (`--ai-backend`, `--model`, `--claude-cli-*`, ...) or `--repo` — \
passing any of them is a clap error. An exported `OMNI_DEV_MODEL` is silently \
ignored. Use `--jev-model` on each subcommand instead to override the Jev model."
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
}

impl JevCommand {
    /// Executes the jev command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            JevSubcommands::Choice(cmd) => cmd.execute().await,
            JevSubcommands::Score(cmd) => cmd.execute().await,
            JevSubcommands::Noul(cmd) => cmd.execute().await,
            JevSubcommands::Ask(cmd) => cmd.execute().await,
        }
    }
}
