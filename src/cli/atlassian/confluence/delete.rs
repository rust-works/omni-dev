//! CLI command for deleting Confluence pages.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::api::AtlassianApi;
use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::confirm::{guard_destructive, GuardOptions, GuardOutcome};
use crate::cli::atlassian::helpers::create_client;

/// Deletes a Confluence page.
#[derive(Parser)]
pub struct DeleteCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be deleted without making any API calls.
    #[arg(long)]
    pub dry_run: bool,

    /// Permanently purges the page instead of moving to trash (requires space admin).
    #[arg(long)]
    pub purge: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);

        if !self.force || self.dry_run {
            let item = api.get_content(&self.id).await?;
            let suffix = if self.purge { " (purge)" } else { "" };
            let prompt = format!("Delete page {} ({}){}? [y/N] ", self.id, item.title, suffix);
            let dry_run_message =
                format!("Would delete page {} ({}){}.", self.id, item.title, suffix);

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

        api.delete_page(&self.id, self.purge).await?;
        println!("Deleted page {} from {}.", self.id, instance_url);

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
            id: "12345".to_string(),
            force: false,
            dry_run: false,
            purge: false,
        };
        assert_eq!(cmd.id, "12345");
        assert!(!cmd.force);
        assert!(!cmd.dry_run);
        assert!(!cmd.purge);
    }

    #[test]
    fn delete_command_force_mode() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        assert!(cmd.force);
    }

    #[test]
    fn delete_command_dry_run_mode() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: true,
            purge: false,
        };
        assert!(cmd.dry_run);
    }

    #[test]
    fn delete_command_purge_mode() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: true,
        };
        assert!(cmd.purge);
    }
}
