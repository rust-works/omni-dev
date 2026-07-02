//! CLI command for reading Confluence pages.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, render_content_item, run_read};

/// Fetches a Confluence page and outputs it as JFM markdown or ADF JSON.
///
/// Any author/version metadata is returned as Atlassian account IDs — resolve
/// them to display names with `omni-dev atlassian confluence user get`.
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

    /// Read a specific historical version instead of the current head
    /// (e.g. `--version 3`). Confluence stores each version as an immutable
    /// snapshot; omit for the latest.
    #[arg(long)]
    pub version: Option<u32>,
}

impl ReadCommand {
    /// Fetches the page and outputs it.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);

        match self.version {
            Some(version) => {
                // `get_page_at_version` is Confluence-specific (not on the shared
                // `AtlassianApi` trait), so fetch here and reuse the shared renderer.
                let item = api.get_page_at_version(&self.id, version).await?;
                render_content_item(&item, self.output.as_deref(), &self.format, &instance_url)
            }
            None => {
                run_read(
                    &self.id,
                    self.output.as_deref(),
                    &self.format,
                    &api,
                    &instance_url,
                )
                .await
            }
        }
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
            version: None,
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.output.as_deref(), Some("page.md"));
        assert!(matches!(cmd.format, ContentFormat::Jfm));
        assert!(cmd.version.is_none());
    }

    #[test]
    fn read_command_adf_format() {
        let cmd = ReadCommand {
            id: "99999".to_string(),
            output: None,
            format: ContentFormat::Adf,
            version: Some(3),
        };
        assert_eq!(cmd.id, "99999");
        assert!(cmd.output.is_none());
        assert!(matches!(cmd.format, ContentFormat::Adf));
        assert_eq!(cmd.version, Some(3));
    }
}
