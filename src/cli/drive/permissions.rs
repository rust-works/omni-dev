//! CLI commands for `omni-dev drive permissions` — read-only diagnostics
//! for the folder-scoped write-permission gate (issue #1574), including the
//! `sheets-write` operation added for the Sheets API (issue #1589).

pub(crate) mod check;
pub(crate) mod lookup_folder;
pub(crate) mod show;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::cli::drive::helpers;

/// Inspects the folder-scoped write-permission rules gating `drive
/// create`/`upload`/`edit` and `drive sheets
/// write`/`append`/`clear`/`create`.
#[derive(Parser)]
pub struct PermissionsCommand {
    /// The permissions subcommand to execute.
    #[command(subcommand)]
    pub command: PermissionsSubcommands,
}

/// Permissions subcommands. Client need is mixed across leaves — `show` is
/// config-only (no network call), `lookup-folder`/`check` both call the
/// Drive API — so, like `Auth`, this command resolves its own client
/// lazily per leaf rather than sharing `DriveCommand::execute`'s single
/// eager resolution.
#[derive(Subcommand)]
pub enum PermissionsSubcommands {
    /// Prints the active account's configured write-permission rules.
    Show(show::ShowCommand),
    /// Searches Drive for candidate folders and prints their ids/paths.
    LookupFolder(lookup_folder::LookupFolderCommand),
    /// Evaluates the configured rules against a real target.
    Check(check::CheckCommand),
}

impl PermissionsCommand {
    pub async fn execute(self) -> Result<()> {
        match self.command {
            PermissionsSubcommands::Show(cmd) => cmd.execute(),
            PermissionsSubcommands::LookupFolder(cmd) => {
                let client = helpers::create_client()?;
                cmd.execute(&client).await
            }
            PermissionsSubcommands::Check(cmd) => {
                let client = helpers::create_client()?;
                cmd.execute(&client).await
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::drive::format::OutputFormat;

    #[test]
    fn permissions_subcommands_show_variant() {
        let cmd = PermissionsCommand {
            command: PermissionsSubcommands::Show(show::ShowCommand {
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, PermissionsSubcommands::Show(_)));
    }
}
