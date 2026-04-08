//! Confluence CLI subcommands.

pub(crate) mod edit;
pub(crate) mod read;
pub(crate) mod write;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Confluence operations.
#[derive(Parser)]
pub struct ConfluenceCommand {
    /// The Confluence subcommand to execute.
    #[command(subcommand)]
    pub command: ConfluenceSubcommands,
}

/// Confluence subcommands.
#[derive(Subcommand)]
pub enum ConfluenceSubcommands {
    /// Fetches a Confluence page and outputs it as JFM markdown or ADF JSON.
    Read(read::ReadCommand),
    /// Pushes content to a Confluence page.
    Write(write::WriteCommand),
    /// Interactive fetch-edit-push cycle for a Confluence page.
    Edit(edit::EditCommand),
}

impl ConfluenceCommand {
    /// Executes the Confluence command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            ConfluenceSubcommands::Read(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Write(cmd) => cmd.execute().await,
            ConfluenceSubcommands::Edit(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::atlassian::format::ContentFormat;

    #[test]
    fn confluence_subcommands_read_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Read(read::ReadCommand {
                id: "12345".to_string(),
                output: None,
                format: ContentFormat::Jfm,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Read(_)));
    }

    #[test]
    fn confluence_subcommands_write_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Write(write::WriteCommand {
                id: "12345".to_string(),
                file: None,
                format: ContentFormat::Adf,
                force: false,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Write(_)));
    }

    #[test]
    fn confluence_subcommands_edit_variant() {
        let cmd = ConfluenceCommand {
            command: ConfluenceSubcommands::Edit(edit::EditCommand {
                id: "12345".to_string(),
            }),
        };
        assert!(matches!(cmd.command, ConfluenceSubcommands::Edit(_)));
    }
}
