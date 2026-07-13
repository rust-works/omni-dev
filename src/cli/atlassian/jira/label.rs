//! CLI commands for incremental JIRA issue label management.
//!
//! Unlike `jira write --set labels=…` (which replaces the whole array), these
//! commands add or remove individual labels via the JIRA `update` verb, leaving
//! the issue's other labels untouched.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::cli::atlassian::helpers::create_client;

/// Adds or removes labels on a JIRA issue incrementally.
#[derive(Parser)]
pub struct LabelCommand {
    /// The label subcommand to execute.
    #[command(subcommand)]
    pub command: LabelSubcommands,
}

/// Label subcommands.
#[derive(Subcommand)]
pub enum LabelSubcommands {
    /// Adds one or more labels to a JIRA issue (mirrors the `jira_label_add` MCP tool).
    Add(AddCommand),
    /// Removes one or more labels from a JIRA issue (mirrors the `jira_label_remove` MCP tool).
    Remove(RemoveCommand),
}

impl LabelCommand {
    /// Executes the label command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            LabelSubcommands::Add(cmd) => cmd.execute().await,
            LabelSubcommands::Remove(cmd) => cmd.execute().await,
        }
    }
}

/// Adds labels to a JIRA issue.
#[derive(Parser)]
pub struct AddCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Labels to add (comma-separated). JIRA labels cannot contain spaces.
    #[arg(long, value_delimiter = ',')]
    pub labels: Vec<String>,
}

impl AddCommand {
    /// Adds the labels.
    pub async fn execute(self) -> Result<()> {
        if self.labels.is_empty() {
            anyhow::bail!("No labels supplied: pass --labels a,b,c");
        }
        let (client, _instance_url) = create_client()?;
        client
            .modify_issue_labels(&self.key, &self.labels, &[])
            .await?;
        println!("Added label(s) {} to {}.", self.labels.join(", "), self.key);
        Ok(())
    }
}

/// Removes labels from a JIRA issue.
#[derive(Parser)]
pub struct RemoveCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Labels to remove (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub labels: Vec<String>,
}

impl RemoveCommand {
    /// Removes the labels.
    pub async fn execute(self) -> Result<()> {
        if self.labels.is_empty() {
            anyhow::bail!("No labels supplied: pass --labels a,b,c");
        }
        let (client, _instance_url) = create_client()?;
        client
            .modify_issue_labels(&self.key, &[], &self.labels)
            .await?;
        println!(
            "Removed label(s) {} from {}.",
            self.labels.join(", "),
            self.key
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn label_command_add_variant() {
        let cmd = LabelCommand {
            command: LabelSubcommands::Add(AddCommand {
                key: "PROJ-1".to_string(),
                labels: vec!["backend".to_string()],
            }),
        };
        assert!(matches!(cmd.command, LabelSubcommands::Add(_)));
    }

    #[test]
    fn label_command_remove_variant() {
        let cmd = LabelCommand {
            command: LabelSubcommands::Remove(RemoveCommand {
                key: "PROJ-1".to_string(),
                labels: vec!["stale".to_string()],
            }),
        };
        assert!(matches!(cmd.command, LabelSubcommands::Remove(_)));
    }

    #[tokio::test]
    async fn add_with_no_labels_errors_before_client() {
        let cmd = AddCommand {
            key: "PROJ-1".to_string(),
            labels: vec![],
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("No labels supplied"));
    }

    #[tokio::test]
    async fn remove_with_no_labels_errors_before_client() {
        let cmd = RemoveCommand {
            key: "PROJ-1".to_string(),
            labels: vec![],
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("No labels supplied"));
    }
}
