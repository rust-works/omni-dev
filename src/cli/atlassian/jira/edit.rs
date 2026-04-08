//! CLI command for interactive JIRA issue editing.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::jira_api::JiraApi;
use crate::cli::atlassian::helpers::{create_client, execute_edit};

/// Interactive fetch-edit-push cycle for a JIRA issue.
#[derive(Parser)]
pub struct EditCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,
}

impl EditCommand {
    /// Fetches the issue, opens in editor, and pushes changes.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = JiraApi::new(client);

        execute_edit(&self.key, &api, &instance_url).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn edit_command_struct_fields() {
        let cmd = EditCommand {
            key: "PROJ-42".to_string(),
        };
        assert_eq!(cmd.key, "PROJ-42");
    }
}
