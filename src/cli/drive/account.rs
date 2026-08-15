//! CLI commands for managing named Drive accounts, orthogonal to
//! `--profile` (issue #1520, [ADR-0069](../../../../docs/adrs/adr-0069.md)).

pub(crate) mod list;
pub(crate) mod set_default;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Manages named Drive accounts configured in `~/.omni-dev/settings.json`.
#[derive(Parser)]
pub struct AccountCommand {
    /// The account subcommand to execute.
    #[command(subcommand)]
    pub command: AccountSubcommands,
}

/// Account subcommands. No `ImportLegacy` — Drive has no pre-existing
/// single-account state to migrate from (ADR-0069 §Consequences).
#[derive(Subcommand)]
pub enum AccountSubcommands {
    /// Lists configured Drive accounts (name/email/scope/default only — no secrets, no network).
    List(list::ListCommand),
    /// Sets the account resolved when `--account`/`OMNI_DEV_DRIVE_ACCOUNT` is not given.
    SetDefault(set_default::SetDefaultCommand),
}

impl AccountCommand {
    /// Executes the account command. Unlike every other Drive subcommand
    /// tree, none of these leaves need a resolved `&DriveClient` — they
    /// manage credential storage, not call the Drive API — so
    /// `DriveCommand::execute` dispatches here directly, the same way it
    /// special-cases `Auth`.
    pub fn execute(self) -> Result<()> {
        match self.command {
            AccountSubcommands::List(cmd) => cmd.execute(),
            AccountSubcommands::SetDefault(cmd) => cmd.execute(),
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
                output: crate::cli::drive::format::OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, AccountSubcommands::List(_)));
    }
}
