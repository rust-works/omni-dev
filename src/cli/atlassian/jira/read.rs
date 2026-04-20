//! CLI command for reading JIRA issues.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::FieldSelection;
use crate::atlassian::jira_api::JiraApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, run_read, run_read_jira_with_fields};

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

    /// Custom fields to include (comma-separated). Each entry may be a
    /// field ID (e.g. `customfield_19300`) or a human name (e.g.
    /// `"Acceptance Criteria"`).
    #[arg(long, value_delimiter = ',')]
    pub fields: Vec<String>,

    /// Include every custom field populated on the issue.
    #[arg(long, conflicts_with = "fields")]
    pub all_fields: bool,
}

impl ReadCommand {
    /// Fetches the issue and outputs it.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;

        let selection = if self.all_fields {
            Some(FieldSelection::All)
        } else if !self.fields.is_empty() {
            Some(FieldSelection::Named(self.fields.clone()))
        } else {
            None
        };

        if let Some(sel) = selection {
            run_read_jira_with_fields(
                &self.key,
                self.output.as_deref(),
                &self.format,
                sel,
                &client,
                &instance_url,
            )
            .await
        } else {
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
            fields: vec![],
            all_fields: false,
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
            fields: vec![],
            all_fields: false,
        };
        assert!(matches!(cmd.format, ContentFormat::Jfm));
        assert!(cmd.output.is_none());
        assert!(cmd.fields.is_empty());
        assert!(!cmd.all_fields);
    }

    #[test]
    fn read_command_with_field_selection() {
        let cmd = ReadCommand {
            key: "PROJ-9".to_string(),
            output: None,
            format: ContentFormat::Jfm,
            fields: vec!["customfield_19300".to_string(), "Sprint".to_string()],
            all_fields: false,
        };
        assert_eq!(cmd.fields.len(), 2);
    }

    #[test]
    fn read_command_all_fields() {
        let cmd = ReadCommand {
            key: "PROJ-9".to_string(),
            output: None,
            format: ContentFormat::Jfm,
            fields: vec![],
            all_fields: true,
        };
        assert!(cmd.all_fields);
    }
}
