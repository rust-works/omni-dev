//! CLI command for creating JIRA issues.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use clap::Parser;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::custom_fields::{
    merge_set_field_overrides, parse_set_field, resolve_custom_fields,
};
use crate::atlassian::document::{CustomFieldSection, JfmDocument, JfmFrontmatter};
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

    /// Set a custom field inline: `--set-field "NAME=VALUE"`. Can be used
    /// multiple times. Values are parsed as YAML scalars (numbers, bools)
    /// when possible, falling back to strings. Overrides values from the
    /// frontmatter `custom_fields:` map for the same name.
    #[arg(long = "set-field", value_name = "NAME=VALUE")]
    pub set_fields: Vec<String>,

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
    custom_scalars: BTreeMap<String, serde_yaml::Value>,
    custom_sections: Vec<CustomFieldSection>,
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
        let overrides = self
            .set_fields
            .iter()
            .map(|s| parse_set_field(s))
            .collect::<Result<Vec<_>>>()?;
        match self.format {
            ContentFormat::Jfm => self.resolve_from_jfm(overrides),
            ContentFormat::Adf => {
                if !overrides.is_empty() {
                    anyhow::bail!(
                        "--set-field is only supported with --format jfm; ADF input takes a raw payload"
                    );
                }
                self.resolve_from_adf()
            }
        }
    }

    /// Resolves parameters from JFM input, with CLI flags as overrides.
    fn resolve_from_jfm(
        &self,
        overrides: Vec<(String, serde_yaml::Value)>,
    ) -> Result<CreateParams> {
        let input = read_input(self.file.as_deref())?;
        let doc = JfmDocument::parse(&input)?;
        let (body_md, custom_sections) = doc.split_custom_sections();
        let adf = markdown_to_adf(&body_md)?;

        let (fm_project, fm_issue_type, fm_summary, fm_labels, fm_scalars) = match &doc.frontmatter
        {
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
                    fm.custom_fields.clone(),
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

        let custom_scalars = merge_set_field_overrides(fm_scalars, overrides);

        Ok(CreateParams {
            project,
            issue_type,
            summary,
            labels: fm_labels,
            adf,
            custom_scalars,
            custom_sections,
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
            custom_scalars: BTreeMap::new(),
            custom_sections: Vec::new(),
        })
    }
}

/// Creates a JIRA issue from resolved parameters.
///
/// Fast path when no custom fields are requested: one POST to
/// `/rest/api/3/issue`. With custom fields, fetches `createmeta` to resolve
/// human names to IDs and dispatch values by schema before sending.
async fn run_create(client: &AtlassianClient, params: &CreateParams) -> Result<()> {
    let custom_fields = if params.custom_scalars.is_empty() && params.custom_sections.is_empty() {
        BTreeMap::new()
    } else {
        let createmeta = client
            .get_createmeta(&params.project, &params.issue_type)
            .await?;
        resolve_custom_fields(&params.custom_scalars, &params.custom_sections, &createmeta)?
    };

    let result = client
        .create_issue_with_custom_fields(
            &params.project,
            &params.issue_type,
            &params.summary,
            Some(&params.adf),
            &params.labels,
            &custom_fields,
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
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
            set_fields: vec![],
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("Confluence"));
    }

    // ── run_create ─────────────────────────────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    fn sample_params() -> CreateParams {
        CreateParams {
            project: "PROJ".to_string(),
            issue_type: "Task".to_string(),
            summary: "Test issue".to_string(),
            labels: vec![],
            adf: AdfDocument::new(),
            custom_scalars: BTreeMap::new(),
            custom_sections: Vec::new(),
        }
    }

    #[tokio::test]
    async fn run_create_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "100", "key": "PROJ-100", "self": "https://org.atlassian.net/rest/api/3/issue/100"
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_create(&client, &sample_params()).await.is_ok());
    }

    #[tokio::test]
    async fn run_create_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad Request"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_create(&client, &sample_params()).await.unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn run_create_with_custom_scalar_fetches_createmeta_and_merges_payload() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .and(wiremock::matchers::query_param("projectKeys", "PROJ"))
            .and(wiremock::matchers::query_param("issuetypeNames", "Task"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "projects": [{
                    "issuetypes": [{
                        "fields": {
                            "customfield_10001": {
                                "name": "Planned / Unplanned Work",
                                "schema": {
                                    "type": "option",
                                    "custom": "com.atlassian.jira.plugin.system.customfieldtypes:select"
                                }
                            }
                        }
                    }]
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "project": {"key": "PROJ"},
                    "issuetype": {"name": "Task"},
                    "summary": "Test issue",
                    "description": {"version": 1, "type": "doc", "content": []},
                    "customfield_10001": {"value": "Unplanned"}
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "100",
                    "key": "PROJ-100",
                    "self": "https://org.atlassian.net/rest/api/3/issue/100"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let mut params = sample_params();
        params.custom_scalars.insert(
            "Planned / Unplanned Work".to_string(),
            serde_yaml::Value::String("Unplanned".to_string()),
        );
        run_create(&client, &params).await.unwrap();
    }

    #[tokio::test]
    async fn run_create_without_custom_fields_skips_createmeta_call() {
        // If no custom fields are requested, create must not hit
        // /rest/api/3/issue/createmeta — only the POST to /issue.
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "100",
                    "key": "PROJ-100",
                    "self": "https://org.atlassian.net/rest/api/3/issue/100"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        // createmeta mock expects exactly 0 hits — wiremock will fail the
        // test if any unmatched GET lands.
        let client = mock_client(&server.uri());
        run_create(&client, &sample_params()).await.unwrap();
    }

    #[test]
    fn resolve_from_jfm_with_set_field_overrides_frontmatter() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: T\ncustom_fields:\n  Priority: Low\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            set_fields: vec!["Priority=High".to_string()],
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(
            params.custom_scalars.get("Priority"),
            Some(&serde_yaml::Value::String("High".to_string()))
        );
    }

    #[test]
    fn invalid_set_field_syntax_errors_during_resolve() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: T\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            project: None,
            r#type: None,
            summary: None,
            set_fields: vec!["no-equals".to_string()],
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("expected --set-field"));
    }

    #[test]
    fn resolve_from_adf_rejects_set_field() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            project: Some("PROJ".to_string()),
            r#type: None,
            summary: Some("T".to_string()),
            set_fields: vec!["Priority=High".to_string()],
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("--set-field is only supported"));
    }
}
