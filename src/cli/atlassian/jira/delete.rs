//! CLI command for deleting JIRA issues.

use anyhow::Result;
use clap::Parser;

use crate::cli::atlassian::confirm::{guard_destructive, GuardOptions, GuardOutcome};
use crate::cli::atlassian::helpers::create_client;

/// Deletes a JIRA issue.
#[derive(Parser)]
pub struct DeleteCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be deleted without making any API calls.
    #[arg(long)]
    pub dry_run: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;

        if !self.force || self.dry_run {
            let issue = client.get_issue(&self.key).await?;
            let prompt = format!("Delete {} ({})? [y/N] ", self.key, issue.summary);
            let dry_run_message = format!("Would delete {} ({}).", self.key, issue.summary);

            let outcome = guard_destructive(&GuardOptions {
                prompt: &prompt,
                dry_run_message: &dry_run_message,
                force: self.force,
                dry_run: self.dry_run,
            })?;

            match outcome {
                GuardOutcome::Cancelled | GuardOutcome::DryRun => return Ok(()),
                GuardOutcome::Proceed => {}
            }
        }

        client.delete_issue(&self.key).await?;
        println!("Deleted {}.", self.key);

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn delete_command_struct_fields() {
        let cmd = DeleteCommand {
            key: "PROJ-42".to_string(),
            force: false,
            dry_run: false,
        };
        assert_eq!(cmd.key, "PROJ-42");
        assert!(!cmd.force);
        assert!(!cmd.dry_run);
    }

    #[test]
    fn delete_command_force_mode() {
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: true,
            dry_run: false,
        };
        assert!(cmd.force);
    }

    #[test]
    fn delete_command_dry_run_mode() {
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: false,
            dry_run: true,
        };
        assert!(cmd.dry_run);
    }
}
