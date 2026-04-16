//! CLI command for creating JIRA issues.

use anyhow::{Context, Result};
use clap::Parser;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::document::{JfmDocument, JfmFrontmatter};
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, print_create_dry_run, read_input};

/// Creates a new JIRA issue.
#[derive(Parser)]
pub struct CreateCommand {
    /// Input file containing JFM markdown or ADF JSON (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Project key (e.g., "PROJ"). Overrides frontmatter.
    #[arg(long)]
    pub project: Option<String>,

    /// Issue type (e.g., "Task", "Bug", "Story"). Overrides frontmatter.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Issue summary/title. Overrides frontmatter.
    #[arg(long)]
    pub summary: Option<String>,

    /// Shows what would be sent without creating.
    #[arg(long)]
    pub dry_run: bool,
}

/// Parameters extracted for issue creation.
#[derive(Debug)]
struct CreateParams {
    project: String,
    issue_type: String,
    summary: String,
    labels: Vec<String>,
    adf: AdfDocument,
}

impl CreateCommand {
    /// Executes the create command.
    pub async fn execute(self) -> Result<()> {
        let params = self.resolve_params()?;

        if self.dry_run {
            return print_create_dry_run(
                &params.project,
                &params.issue_type,
                &params.summary,
                &params.adf,
                &params.labels,
            );
        }

        let (client, _instance_url) = create_client()?;
        run_create(&client, &params).await
    }

    /// Resolves creation parameters from input file and CLI flags.
    fn resolve_params(&self) -> Result<CreateParams> {
        match self.format {
            ContentFormat::Jfm => self.resolve_from_jfm(),
            ContentFormat::Adf => self.resolve_from_adf(),
        }
    }

    /// Resolves parameters from JFM input, with CLI flags as overrides.
    fn resolve_from_jfm(&self) -> Result<CreateParams> {
        let input = read_input(self.file.as_deref())?;
        let doc = JfmDocument::parse(&input)?;
        let adf = markdown_to_adf(&doc.body)?;

        let (fm_project, fm_issue_type, fm_summary, fm_labels) = match &doc.frontmatter {
            JfmFrontmatter::Jira(fm) => {
                // Derive project from key if project field is absent
                let project = fm.project.clone().or_else(|| {
                    if fm.key.is_empty() {
                        None
                    } else {
                        fm.key.split('-').next().map(String::from)
                    }
                });
                (
                    project,
                    fm.issue_type.clone(),
                    Some(fm.summary.clone()),
                    fm.labels.clone(),
                )
            }
            JfmFrontmatter::Confluence(_) => {
                anyhow::bail!("Cannot create a JIRA issue from Confluence frontmatter");
            }
        };

        let project = self.project.clone().or(fm_project).ok_or_else(|| {
            anyhow::anyhow!("Project key is required (use --project or set in frontmatter)")
        })?;

        let issue_type = self
            .r#type
            .clone()
            .or(fm_issue_type)
            .unwrap_or_else(|| "Task".to_string());

        let summary = self.summary.clone().or(fm_summary).ok_or_else(|| {
            anyhow::anyhow!("Summary is required (use --summary or set in frontmatter)")
        })?;

        Ok(CreateParams {
            project,
            issue_type,
            summary,
            labels: fm_labels,
            adf,
        })
    }

    /// Resolves parameters from ADF input — all metadata must come from CLI flags.
    fn resolve_from_adf(&self) -> Result<CreateParams> {
        let input = read_input(self.file.as_deref())?;
        let adf: AdfDocument =
            serde_json::from_str(&input).context("Failed to parse ADF JSON input")?;

        let project = self
            .project
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--project is required when using ADF format"))?;

        let issue_type = self.r#type.clone().unwrap_or_else(|| "Task".to_string());

        let summary = self
            .summary
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--summary is required when using ADF format"))?;

        Ok(CreateParams {
            project,
            issue_type,
            summary,
            labels: vec![],
            adf,
        })
    }
}

/// Creates a JIRA issue from resolved parameters.
async fn run_create(client: &AtlassianClient, params: &CreateParams) -> Result<()> {
    let result = client
        .create_issue(
            &params.project,
            &params.issue_type,
            &params.summary,
            Some(&params.adf),
            &params.labels,
        )
        .await?;

    println!("{}", result.key);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn create_command_struct_defaults() {
        let cmd = CreateCommand {
            file: None,
            format: ContentFormat::default(),
            project: None,
            r#type: None,
            summary: None,
            dry_run: false,
        };
        assert!(cmd.file.is_none());
        assert!(cmd.project.is_none());
        assert!(!cmd.dry_run);
    }

    #[test]
    fn resolve_from_jfm_with_all_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: My Title\nissue_type: Bug\nlabels:\n  - backend\n---\n\nBody text\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.project, "PROJ");
        assert_eq!(params.issue_type, "Bug");
        assert_eq!(params.summary, "My Title");
        assert_eq!(params.labels, vec!["backend"]);
    }

    #[test]
    fn resolve_from_jfm_project_from_key() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-123\nsummary: Existing issue\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.project, "PROJ");
    }

    #[test]
    fn cli_flags_override_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: OLD\nsummary: Old Title\nissue_type: Bug\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: Some("NEW".to_string()),
            r#type: Some("Story".to_string()),
            summary: Some("New Title".to_string()),
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.project, "NEW");
        assert_eq!(params.issue_type, "Story");
        assert_eq!(params.summary, "New Title");
    }

    #[test]
    fn resolve_from_jfm_missing_project_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nsummary: No project\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("Project key is required"));
    }

    #[test]
    fn resolve_from_adf_with_flags() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            project: Some("PROJ".to_string()),
            r#type: Some("Bug".to_string()),
            summary: Some("Fix it".to_string()),
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.project, "PROJ");
        assert_eq!(params.issue_type, "Bug");
        assert_eq!(params.summary, "Fix it");
    }

    #[test]
    fn resolve_from_adf_missing_project_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            project: None,
            r#type: None,
            summary: Some("Title".to_string()),
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("--project is required"));
    }

    #[test]
    fn resolve_from_adf_missing_summary_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            project: Some("PROJ".to_string()),
            r#type: None,
            summary: None,
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("--summary is required"));
    }

    #[test]
    fn default_issue_type_is_task() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: Title\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.issue_type, "Task");
    }

    #[test]
    fn dry_run_jfm_does_not_call_api() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: Test\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn dry_run_adf_does_not_call_api() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            project: Some("PROJ".to_string()),
            r#type: None,
            summary: Some("Test".to_string()),
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn confluence_frontmatter_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("page.md");
        let content = "---\ntype: confluence\ninstance: https://org.atlassian.net\npage_id: '12345'\ntitle: Page\nspace_key: ENG\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("Confluence"));
    }
}
