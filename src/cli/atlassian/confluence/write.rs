//! CLI command for writing content to Confluence pages.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, prepare_write, run_write};

/// Pushes content to a Confluence page.
#[derive(Parser)]
pub struct WriteCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Input file (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Shows what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
}

impl WriteCommand {
    /// Reads input and pushes to the Confluence page.
    pub async fn execute(self) -> Result<()> {
        let (adf, title) = prepare_write(self.file.as_deref(), &self.format)?;

        if self.dry_run {
            return crate::cli::atlassian::helpers::print_dry_run(&self.id, &adf, &title);
        }

        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);

        run_write(&self.id, &adf, &title, self.force, &api).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn write_command_struct_fields() {
        let cmd = WriteCommand {
            id: "12345".to_string(),
            file: Some("page.md".to_string()),
            format: ContentFormat::Jfm,
            force: false,
            dry_run: true,
        };
        assert_eq!(cmd.id, "12345");
        assert!(!cmd.force);
        assert!(cmd.dry_run);
    }

    #[test]
    fn dry_run_adf_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("page.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = WriteCommand {
            id: "12345".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn dry_run_confluence_document() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("page.md");
        let content = "---\ntype: confluence\ninstance: https://org.atlassian.net\npage_id: '12345'\ntitle: My Page\nspace_key: ENG\n---\n\nPage body\n";
        fs::write(&file_path, content).unwrap();

        let cmd = WriteCommand {
            id: "12345".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }
}
