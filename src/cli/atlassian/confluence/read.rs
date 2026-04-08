//! CLI command for reading Confluence pages.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, execute_read};

/// Fetches a Confluence page and outputs it as JFM markdown or ADF JSON.
#[derive(Parser)]
pub struct ReadCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Output file (writes to stdout if omitted).
    #[arg(short, long)]
    pub output: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,
}

impl ReadCommand {
    /// Fetches the page and outputs it.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);

        execute_read(
            &self.id,
            self.output.as_deref(),
            &self.format,
            &api,
            &instance_url,
        )
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn read_command_struct_fields() {
        let cmd = ReadCommand {
            id: "12345".to_string(),
            output: Some("page.md".to_string()),
            format: ContentFormat::Jfm,
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.output.as_deref(), Some("page.md"));
        assert!(matches!(cmd.format, ContentFormat::Jfm));
    }

    #[test]
    fn read_command_adf_format() {
        let cmd = ReadCommand {
            id: "99999".to_string(),
            output: None,
            format: ContentFormat::Adf,
        };
        assert_eq!(cmd.id, "99999");
        assert!(cmd.output.is_none());
        assert!(matches!(cmd.format, ContentFormat::Adf));
    }
}
