//! CLI commands for managing named Gmail accounts, orthogonal to
//! `--profile` (issue #1500,
//! [ADR-0066](../../../../docs/adrs/adr-0066.md)).

pub(crate) mod import_legacy;
pub(crate) mod list;
pub(crate) mod set_default;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Manages named Gmail accounts configured in `~/.omni-dev/settings.json`.
#[derive(Parser)]
pub struct AccountCommand {
    /// The account subcommand to execute.
    #[command(subcommand)]
    pub command: AccountSubcommands,
}

/// Account subcommands.
#[derive(Subcommand)]
pub enum AccountSubcommands {
    /// Lists configured Gmail accounts (name/email/scope/default only — no secrets, no network).
    List(list::ListCommand),
    /// Sets the account resolved when `--account`/`OMNI_DEV_GMAIL_ACCOUNT` is not given.
    SetDefault(set_default::SetDefaultCommand),
    /// Migrates today's legacy (profile/base `env`) Gmail credentials into a named account.
    ImportLegacy(import_legacy::ImportLegacyCommand),
}

impl AccountCommand {
    /// Executes the account command. Unlike every other Gmail subcommand
    /// tree, none of these leaves need a resolved `&GmailClient` — they
    /// manage credential storage, not call the Gmail API — so
    /// `GmailCommand::execute` dispatches here directly, the same way it
    /// special-cases `Auth`.
    pub fn execute(self) -> Result<()> {
        match self.command {
            AccountSubcommands::List(cmd) => cmd.execute(),
            AccountSubcommands::SetDefault(cmd) => cmd.execute(),
            AccountSubcommands::ImportLegacy(cmd) => cmd.execute(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn account_subcommands_list_variant() {
        let cmd = AccountCommand {
            command: AccountSubcommands::List(list::ListCommand {
                output: crate::cli::gmail::format::OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, AccountSubcommands::List(_)));
    }
}
