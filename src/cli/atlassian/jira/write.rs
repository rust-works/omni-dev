//! CLI command for writing content to JIRA issues.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::custom_fields::{
    merge_set_field_overrides, parse_set_field, resolve_custom_fields,
};
use crate::atlassian::document::JfmDocument;
use crate::atlassian::jira_api::JiraApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{
    create_client, prepare_write, print_dry_run, print_jira_dry_run_with_custom_fields, read_input,
    run_write, run_write_jira_with_resolved_fields,
};

/// Pushes content to a JIRA issue.
#[derive(Parser)]
pub struct WriteCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Input file (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Set a custom field inline: `--set-field "NAME=VALUE"`. Can be used
    /// multiple times. Values are parsed as YAML scalars (numbers, bools)
    /// when possible, falling back to strings. Overrides values from the
    /// frontmatter `custom_fields:` map for the same name.
    #[arg(long = "set-field", value_name = "NAME=VALUE")]
    pub set_fields: Vec<String>,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Shows what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
}

impl WriteCommand {
    /// Reads input and pushes to the JIRA issue.
    pub async fn execute(self) -> Result<()> {
        let overrides = self
            .set_fields
            .iter()
            .map(|s| parse_set_field(s))
            .collect::<Result<Vec<_>>>()?;

        if matches!(self.format, ContentFormat::Adf) {
            if !overrides.is_empty() {
                anyhow::bail!(
                    "--set-field is only supported with --format jfm; ADF writes take a raw payload"
                );
            }
            let (adf, title) = prepare_write(self.file.as_deref(), &self.format)?;
            if self.dry_run {
                return print_dry_run(&self.key, &adf, &title);
            }
            let (client, _instance_url) = create_client()?;
            let api = JiraApi::new(client);
            return run_write(&self.key, &adf, &title, self.force, &api).await;
        }

        // JFM path: may carry custom fields in frontmatter or body sections.
        let input = read_input(self.file.as_deref())?;
        let doc = JfmDocument::parse(&input)?;
        let (body_md, sections) = doc.split_custom_sections();
        let frontmatter_scalars = doc
            .frontmatter
            .jira_custom_fields()
            .cloned()
            .unwrap_or_default();
        let scalars = merge_set_field_overrides(frontmatter_scalars, overrides);
        let body_adf = markdown_to_adf(&body_md)?;
        let title = doc.frontmatter.title().to_string();

        if self.dry_run {
            return print_jira_dry_run_with_custom_fields(
                &self.key, &body_adf, &title, &scalars, &sections,
            );
        }

        let (client, _instance_url) = create_client()?;

        if scalars.is_empty() && sections.is_empty() {
            let api = JiraApi::new(client);
            return run_write(&self.key, &body_adf, &title, self.force, &api).await;
        }

        let editmeta = client.get_editmeta(&self.key).await?;
        let resolved = resolve_custom_fields(&scalars, &sections, &editmeta)?;

        run_write_jira_with_resolved_fields(
            &self.key, &body_adf, &title, self.force, &resolved, &client,
        )
        .await
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
            key: "PROJ-1".to_string(),
            file: Some("input.md".to_string()),
            format: ContentFormat::Jfm,
            set_fields: vec![],
            force: true,
            dry_run: false,
        };
        assert_eq!(cmd.key, "PROJ-1");
        assert!(cmd.force);
        assert!(!cmd.dry_run);
    }

    #[test]
    fn dry_run_does_not_call_api() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Test\n---\n\nBody content\n";
        fs::write(&file_path, content).unwrap();

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            set_fields: vec![],
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn set_field_with_adf_format_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.json");
        fs::write(&file_path, r#"{"version":1,"type":"doc","content":[]}"#).unwrap();

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            set_fields: vec!["Priority=High".to_string()],
            force: true,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(cmd.execute()).unwrap_err();
        assert!(err
            .to_string()
            .contains("--set-field is only supported with --format jfm"));
    }

    #[test]
    fn invalid_set_field_syntax_errors() {
        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: None,
            format: ContentFormat::Jfm,
            set_fields: vec!["no-equals-sign".to_string()],
            force: true,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(cmd.execute()).unwrap_err();
        assert!(err.to_string().contains("expected --set-field"));
    }

    #[test]
    fn dry_run_with_set_field_prints_custom_fields() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content =
            "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: T\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            set_fields: vec!["Priority=High".to_string()],
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(cmd.execute()).is_ok());
    }

    #[test]
    fn dry_run_adf_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            set_fields: vec![],
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }
}
