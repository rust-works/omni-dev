//! CLI command for interactive Confluence page editing.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::helpers::{create_client, execute_edit};

/// Interactive fetch-edit-push cycle for a Confluence page.
#[derive(Parser)]
pub struct EditCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,
}

impl EditCommand {
    /// Fetches the page, opens in editor, and pushes changes.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);

        execute_edit(&self.id, &api, &instance_url).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn edit_command_struct_fields() {
        let cmd = EditCommand {
            id: "12345".to_string(),
        };
        assert_eq!(cmd.id, "12345");
    }
}
