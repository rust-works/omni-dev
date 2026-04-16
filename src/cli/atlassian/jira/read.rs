//! CLI command for reading JIRA issues.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::jira_api::JiraApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, run_read};

/// Fetches a JIRA issue and outputs it as JFM markdown or ADF JSON.
#[derive(Parser)]
pub struct ReadCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output file (writes to stdout if omitted).
    #[arg(short, long)]
    pub output: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,
}

impl ReadCommand {
    /// Fetches the issue and outputs it.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = JiraApi::new(client);

        run_read(
            &self.key,
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
            key: "PROJ-42".to_string(),
            output: Some("out.md".to_string()),
            format: ContentFormat::Adf,
        };
        assert_eq!(cmd.key, "PROJ-42");
        assert_eq!(cmd.output.as_deref(), Some("out.md"));
        assert!(matches!(cmd.format, ContentFormat::Adf));
    }

    #[test]
    fn read_command_default_format() {
        let cmd = ReadCommand {
            key: "PROJ-1".to_string(),
            output: None,
            format: ContentFormat::default(),
        };
        assert!(matches!(cmd.format, ContentFormat::Jfm));
        assert!(cmd.output.is_none());
    }
}
