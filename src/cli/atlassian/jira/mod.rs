//! JIRA CLI subcommands.

pub(crate) mod edit;
pub(crate) mod read;
pub(crate) mod write;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// JIRA operations.
#[derive(Parser)]
pub struct JiraCommand {
    /// The JIRA subcommand to execute.
    #[command(subcommand)]
    pub command: JiraSubcommands,
}

/// JIRA subcommands.
#[derive(Subcommand)]
pub enum JiraSubcommands {
    /// Fetches a JIRA issue and outputs it as JFM markdown or ADF JSON.
    Read(read::ReadCommand),
    /// Pushes content to a JIRA issue.
    Write(write::WriteCommand),
    /// Interactive fetch-edit-push cycle for a JIRA issue.
    Edit(edit::EditCommand),
}

impl JiraCommand {
    /// Executes the JIRA command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            JiraSubcommands::Read(cmd) => cmd.execute().await,
            JiraSubcommands::Write(cmd) => cmd.execute().await,
            JiraSubcommands::Edit(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::atlassian::format::ContentFormat;

    #[test]
    fn jira_subcommands_read_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Read(read::ReadCommand {
                key: "PROJ-1".to_string(),
                output: None,
                format: ContentFormat::Jfm,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Read(_)));
    }

    #[test]
    fn jira_subcommands_write_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Write(write::WriteCommand {
                key: "PROJ-1".to_string(),
                file: None,
                format: ContentFormat::Jfm,
                force: false,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Write(_)));
    }

    #[test]
    fn jira_subcommands_edit_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Edit(edit::EditCommand {
                key: "PROJ-1".to_string(),
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Edit(_)));
    }
}
