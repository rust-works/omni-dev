//! JIRA CLI subcommands.

pub(crate) mod create;
pub(crate) mod edit;
pub(crate) mod read;
pub(crate) mod search;
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
    /// Searches JIRA issues using JQL.
    Search(search::SearchCommand),
    /// Creates a new JIRA issue.
    Create(create::CreateCommand),
}

impl JiraCommand {
    /// Executes the JIRA command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            JiraSubcommands::Read(cmd) => cmd.execute().await,
            JiraSubcommands::Write(cmd) => cmd.execute().await,
            JiraSubcommands::Edit(cmd) => cmd.execute().await,
            JiraSubcommands::Search(cmd) => cmd.execute().await,
            JiraSubcommands::Create(cmd) => cmd.execute().await,
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

    #[test]
    fn jira_subcommands_create_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Create(create::CreateCommand {
                file: None,
                format: ContentFormat::Jfm,
                project: Some("PROJ".to_string()),
                r#type: None,
                summary: Some("Test".to_string()),
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Create(_)));
    }

    #[test]
    fn jira_subcommands_search_variant() {
        let cmd = JiraCommand {
            command: JiraSubcommands::Search(search::SearchCommand {
                jql: Some("project = PROJ".to_string()),
                project: None,
                assignee: None,
                status: None,
                max_results: 50,
            }),
        };
        assert!(matches!(cmd.command, JiraSubcommands::Search(_)));
    }
}
