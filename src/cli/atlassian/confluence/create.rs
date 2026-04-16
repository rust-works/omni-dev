//! CLI command for creating Confluence pages.

use anyhow::{Context, Result};
use clap::Parser;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::confluence_api::ConfluenceApi;
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::document::{JfmDocument, JfmFrontmatter};
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, read_input};

/// Creates a new Confluence page.
#[derive(Parser)]
pub struct CreateCommand {
    /// Input file containing JFM markdown or ADF JSON (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Space key (e.g., "ENG"). Overrides frontmatter.
    #[arg(long)]
    pub space: Option<String>,

    /// Page title. Overrides frontmatter.
    #[arg(long)]
    pub title: Option<String>,

    /// Parent page ID for nesting under an existing page.
    #[arg(long)]
    pub parent: Option<String>,

    /// Shows what would be sent without creating.
    #[arg(long)]
    pub dry_run: bool,
}

/// Parameters extracted for page creation.
#[derive(Debug)]
struct CreateParams {
    space: String,
    title: String,
    parent_id: Option<String>,
    adf: AdfDocument,
}

impl CreateCommand {
    /// Executes the create command.
    pub async fn execute(self) -> Result<()> {
        let params = self.resolve_params()?;

        if self.dry_run {
            return print_create_dry_run(&params);
        }

        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        run_create(&api, &params).await
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

        let (fm_space, fm_title, fm_parent) = match &doc.frontmatter {
            JfmFrontmatter::Confluence(fm) => (
                Some(fm.space_key.clone()),
                Some(fm.title.clone()),
                fm.parent_id.clone(),
            ),
            JfmFrontmatter::Jira(_) => {
                anyhow::bail!("Cannot create a Confluence page from JIRA frontmatter");
            }
        };

        let space = self.space.clone().or(fm_space).ok_or_else(|| {
            anyhow::anyhow!("Space key is required (use --space or set in frontmatter)")
        })?;

        let title = self.title.clone().or(fm_title).ok_or_else(|| {
            anyhow::anyhow!("Title is required (use --title or set in frontmatter)")
        })?;

        let parent_id = self.parent.clone().or(fm_parent);

        Ok(CreateParams {
            space,
            title,
            parent_id,
            adf,
        })
    }

    /// Resolves parameters from ADF input — all metadata must come from CLI flags.
    fn resolve_from_adf(&self) -> Result<CreateParams> {
        let input = read_input(self.file.as_deref())?;
        let adf: AdfDocument =
            serde_json::from_str(&input).context("Failed to parse ADF JSON input")?;

        let space = self
            .space
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--space is required when using ADF format"))?;

        let title = self
            .title
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--title is required when using ADF format"))?;

        Ok(CreateParams {
            space,
            title,
            parent_id: self.parent.clone(),
            adf,
        })
    }
}

/// Creates a Confluence page from resolved parameters.
async fn run_create(api: &ConfluenceApi, params: &CreateParams) -> Result<()> {
    let page_id = api
        .create_page(
            &params.space,
            &params.title,
            &params.adf,
            params.parent_id.as_deref(),
        )
        .await?;

    println!("{page_id}");
    Ok(())
}

/// Prints a dry-run summary for page creation.
fn print_create_dry_run(params: &CreateParams) -> Result<()> {
    println!("Dry run — would create page:");
    println!("  Space:      {}", params.space);
    println!("  Title:      {}", params.title);
    if let Some(ref parent) = params.parent_id {
        println!("  Parent ID:  {parent}");
    }
    println!(
        "\nADF body:\n{}",
        serde_json::to_string_pretty(&params.adf).context("Failed to serialize ADF")?
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    // ── resolve_params ─────────────────────────────────────────────

    #[test]
    fn resolve_from_jfm_with_all_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("page.md");
        let content = "---\ntype: confluence\ninstance: https://org.atlassian.net\ntitle: My Page\nspace_key: ENG\nparent_id: '11111'\n---\n\nPage body\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            space: None,
            title: None,
            parent: None,
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.space, "ENG");
        assert_eq!(params.title, "My Page");
        assert_eq!(params.parent_id.as_deref(), Some("11111"));
    }

    #[test]
    fn resolve_from_jfm_without_parent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("page.md");
        let content = "---\ntype: confluence\ninstance: https://org.atlassian.net\ntitle: Root Page\nspace_key: DOC\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            space: None,
            title: None,
            parent: None,
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.space, "DOC");
        assert!(params.parent_id.is_none());
    }

    #[test]
    fn cli_flags_override_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("page.md");
        let content = "---\ntype: confluence\ninstance: https://org.atlassian.net\ntitle: Old Title\nspace_key: OLD\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            space: Some("NEW".to_string()),
            title: Some("New Title".to_string()),
            parent: Some("99999".to_string()),
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.space, "NEW");
        assert_eq!(params.title, "New Title");
        assert_eq!(params.parent_id.as_deref(), Some("99999"));
    }

    #[test]
    fn jira_frontmatter_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Test\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            space: None,
            title: None,
            parent: None,
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("JIRA"));
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
            space: Some("ENG".to_string()),
            title: Some("New Page".to_string()),
            parent: None,
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.space, "ENG");
        assert_eq!(params.title, "New Page");
    }

    #[test]
    fn resolve_from_adf_missing_space_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        fs::write(&file_path, r#"{"version":1,"type":"doc","content":[]}"#).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            space: None,
            title: Some("Title".to_string()),
            parent: None,
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("--space"));
    }

    #[test]
    fn resolve_from_adf_missing_title_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        fs::write(&file_path, r#"{"version":1,"type":"doc","content":[]}"#).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            space: Some("ENG".to_string()),
            title: None,
            parent: None,
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("--title"));
    }

    // ── dry run ────────────────────────────────────────────────────

    #[test]
    fn dry_run_jfm() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("page.md");
        let content = "---\ntype: confluence\ninstance: https://org.atlassian.net\ntitle: Test\nspace_key: ENG\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            space: None,
            title: None,
            parent: None,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn dry_run_adf() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        fs::write(&file_path, r#"{"version":1,"type":"doc","content":[]}"#).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            space: Some("ENG".to_string()),
            title: Some("Test".to_string()),
            parent: None,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    // ── print_create_dry_run ───────────────────────────────────────

    #[test]
    fn dry_run_with_parent() {
        let params = CreateParams {
            space: "ENG".to_string(),
            title: "Child Page".to_string(),
            parent_id: Some("11111".to_string()),
            adf: AdfDocument::new(),
        };
        assert!(print_create_dry_run(&params).is_ok());
    }

    #[test]
    fn dry_run_without_parent() {
        let params = CreateParams {
            space: "ENG".to_string(),
            title: "Root Page".to_string(),
            parent_id: None,
            adf: AdfDocument::new(),
        };
        assert!(print_create_dry_run(&params).is_ok());
    }

    // ── struct fields ──────────────────────────────────────────────

    #[test]
    fn create_command_defaults() {
        let cmd = CreateCommand {
            file: None,
            format: ContentFormat::default(),
            space: None,
            title: None,
            parent: None,
            dry_run: false,
        };
        assert!(cmd.file.is_none());
        assert!(cmd.space.is_none());
        assert!(!cmd.dry_run);
    }
}
