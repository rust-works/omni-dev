//! Atlassian CLI commands for JIRA and Confluence.

pub(crate) mod auth;
pub mod confluence;
pub(crate) mod convert;
pub(crate) mod format;
pub(crate) mod helpers;
pub mod jira;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Atlassian: JIRA and Confluence operations.
#[derive(Parser)]
pub struct AtlassianCommand {
    /// The Atlassian subcommand to execute.
    #[command(subcommand)]
    pub command: AtlassianSubcommands,
}

/// Atlassian subcommands.
#[derive(Subcommand)]
pub enum AtlassianSubcommands {
    /// JIRA issue management, search, agile boards, and more.
    Jira(jira::JiraCommand),
    /// Confluence page management, search, and more.
    Confluence(confluence::ConfluenceCommand),
    /// Converts between JFM markdown and ADF JSON.
    Convert(convert::ConvertCommand),
    /// Manages Atlassian Cloud credentials.
    Auth(auth::AuthCommand),
}

impl AtlassianCommand {
    /// Executes the Atlassian command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            AtlassianSubcommands::Jira(cmd) => cmd.execute().await,
            AtlassianSubcommands::Confluence(cmd) => cmd.execute().await,
            AtlassianSubcommands::Convert(cmd) => cmd.execute(),
            AtlassianSubcommands::Auth(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlassian_subcommands_jira_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Jira(jira::JiraCommand {
                command: jira::JiraSubcommands::Edit(jira::edit::EditCommand {
                    key: "PROJ-1".to_string(),
                }),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Jira(_)));
    }

    #[test]
    fn atlassian_subcommands_confluence_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Confluence(confluence::ConfluenceCommand {
                command: confluence::ConfluenceSubcommands::Edit(confluence::edit::EditCommand {
                    id: "12345".to_string(),
                }),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Confluence(_)));
    }

    #[test]
    fn atlassian_subcommands_auth_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Login(auth::LoginCommand),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Auth(_)));
    }

    #[test]
    fn atlassian_subcommands_convert_variant() {
        let cmd = AtlassianCommand {
            command: AtlassianSubcommands::Convert(convert::ConvertCommand {
                command: convert::ConvertSubcommands::FromAdf(convert::FromAdfCommand {
                    file: None,
                }),
            }),
        };
        assert!(matches!(cmd.command, AtlassianSubcommands::Convert(_)));
    }
}
