//! CLI command for creating JIRA issues.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use clap::Parser;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::create::{create_resolved_jira_issue, resolve_jira_create};
use crate::atlassian::custom_fields::parse_set_field;
use crate::atlassian::document::CustomFieldSection;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{
    create_client_with_instance, print_create_dry_run, read_input,
};

/// `--help` epilogue documenting the fields accepted only via JFM frontmatter,
/// so they are discoverable without first triggering a runtime error.
const CREATE_AFTER_HELP: &str = "\
JFM FRONTMATTER (--format jfm):
  These fields are read from the document's YAML frontmatter. The flags above,
  when given, override the matching frontmatter field:
    project        project key (or derived from `key:` for an existing issue)
    summary        issue title
    issue_type     issue type (overridden by --type)
    labels         list of labels
    custom_fields  map of custom field name -> value (overridden by --set-field)
    instance       OPTIONAL, informational only; NOT used for routing. The
                   target instance comes from --instance, else
                   ATLASSIAN_INSTANCE_URL / settings.json.

ADF INPUT (--format adf):
  The body is a raw ADF payload; supply --project and --summary as flags.";

/// Creates a new JIRA issue.
///
/// Metadata is resolved with this precedence: CLI flags first, then JFM
/// frontmatter, then a derived or default value. Flags always win.
///
/// Frontmatter is optional. A file with no `---` block is treated entirely as
/// the issue body, with every field taken from flags (the same way the MCP
/// `jira_create` tool works) — so `--project` and `--summary` (plus an
/// optional `--type`) are enough to create an issue from a plain markdown
/// body. No `instance` is needed: the target is taken from auth config.
#[derive(Parser)]
#[command(after_help = CREATE_AFTER_HELP)]
pub struct CreateCommand {
    /// Input file containing JFM markdown or ADF JSON (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Atlassian instance URL (e.g. "https://org.atlassian.net"). Overrides
    /// ATLASSIAN_INSTANCE_URL / settings.json for this invocation.
    #[arg(long, value_name = "URL")]
    pub instance: Option<String>,

    /// Project key (e.g., "PROJ"). Overrides frontmatter.
    #[arg(long)]
    pub project: Option<String>,

    /// Issue type (e.g., "Task", "Bug", "Story"). Overrides frontmatter.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Issue summary/title. Overrides frontmatter.
    #[arg(long)]
    pub summary: Option<String>,

    /// Set a custom field inline, e.g. `--set-field "Story Points=5"`.
    /// Repeatable.
    ///
    /// Split on the FIRST `=`: NAME is everything before it (surrounding
    /// spaces trimmed), VALUE everything after — spaces, parentheses, and
    /// further `=` are kept verbatim, so only your shell's quoting is needed
    /// (e.g. `--set-field "Work Type=Product Features (Planned)"`).
    ///
    /// VALUE is parsed as a YAML scalar (number, bool) when possible, else a
    /// string; the wire type then follows the field's schema — option fields
    /// send `{"value": ...}`, number fields send a number, and so on. Array
    /// fields (labels, components, versions) accept comma-separated values
    /// (`Labels=a,b,c`) or a YAML list (`Labels=[a, b]`).
    ///
    /// NAME resolves against the project + issue-type create screen: a
    /// `customfield_<digits>` id matches first, otherwise the display name.
    /// The field must be on the create screen or the command errors listing
    /// the accepted fields; for option fields a value outside the allowed set
    /// is rejected up front. Run `omni-dev atlassian jira project create-meta`
    /// to list accepted fields, their types, and allowed values. Overrides the
    /// frontmatter `custom_fields:` entry of the same name.
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
    adf: ValidatedAdfDocument,
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

        let (client, _instance_url) = create_client_with_instance(self.instance.as_deref())?;
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
    ///
    /// Frontmatter is optional: an input with no `---` block is treated
    /// entirely as the issue body, with all metadata sourced from flags
    /// (parity with the MCP `jira_create` tool). Resolution precedence is
    /// flags → frontmatter → derived/default.
    fn resolve_from_jfm(
        &self,
        overrides: Vec<(String, serde_yaml::Value)>,
    ) -> Result<CreateParams> {
        let input = read_input(self.file.as_deref())?;
        let resolved = resolve_jira_create(
            &input,
            self.project.as_deref(),
            self.summary.as_deref(),
            self.r#type.as_deref(),
            overrides,
        )?;
        for shadowed in &resolved.shadowed {
            eprintln!("{}", shadowed.warning_line());
        }

        Ok(CreateParams {
            project: resolved.project,
            issue_type: resolved.issue_type,
            summary: resolved.summary,
            labels: resolved.labels,
            adf: resolved.adf,
            custom_scalars: resolved.custom_scalars,
            custom_sections: resolved.custom_sections,
        })
    }

    /// Resolves parameters from ADF input — all metadata must come from CLI flags.
    fn resolve_from_adf(&self) -> Result<CreateParams> {
        let input = read_input(self.file.as_deref())?;
        let adf: AdfDocument =
            serde_json::from_str(&input).context("Failed to parse ADF JSON input")?;
        let adf = ValidatedAdfDocument::try_new(adf)?;

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

/// Creates a JIRA issue from resolved parameters via the shared
/// [`create_resolved_jira_issue`] helper (which handles the `createmeta`
/// custom-field resolution path).
async fn run_create(client: &AtlassianClient, params: &CreateParams) -> Result<()> {
    let result = create_resolved_jira_issue(
        client,
        &params.project,
        &params.issue_type,
        &params.summary,
        &params.adf,
        &params.labels,
        &params.custom_scalars,
        &params.custom_sections,
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
            instance: None,
            project: None,
            r#type: None,
            summary: None,
            set_fields: vec![],
            dry_run: false,
        };
        assert!(cmd.file.is_none());
        assert!(cmd.instance.is_none());
        assert!(cmd.project.is_none());
        assert!(!cmd.dry_run);
    }

    #[test]
    fn resolve_from_jfm_without_instance_frontmatter_succeeds() {
        // Issue #1051: `instance` is no longer a required frontmatter field;
        // a JFM doc that omits it must resolve rather than failing with
        // "missing field `instance`". Routing is independent of this field.
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\nproject: PROJ\nsummary: No instance here\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: None,
            r#type: None,
            summary: None,
            set_fields: vec![],
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.project, "PROJ");
        assert_eq!(params.summary, "No instance here");
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
            instance: None,
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
            instance: None,
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
            instance: None,
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
    fn resolve_from_jfm_rejects_invalid_adf_nesting() {
        // Issue #714: issue body that produces invalid ADF (panel→expand)
        // must be rejected at resolve time, before the API call.
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Bad\nproject: PROJ\nissuetype: Task\n---\n\n:::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: None,
            r#type: None,
            summary: None,
            set_fields: vec![],
            dry_run: false,
        };
        let err = cmd.resolve_params().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }

    #[test]
    fn resolve_from_jfm_rejects_forbidden_mark_combination() {
        // Issue #1047: a body with `**`text`**` produces a text node carrying
        // both `strong` and `code` marks, which ADF rejects. It must be
        // caught at resolve time, before the API call.
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nproject: PROJ\nsummary: Bad\n---\n\nThis is **`bolded code`** in a sentence.\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: None,
            r#type: None,
            summary: None,
            set_fields: vec![],
            dry_run: false,
        };
        let err = cmd.resolve_params().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF mark combination"), "got: {msg}");
        assert!(msg.contains("cannot be combined with"), "got: {msg}");
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
            instance: None,
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
            instance: None,
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
            instance: None,
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
            instance: None,
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
            instance: None,
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
    fn resolve_from_jfm_no_frontmatter_uses_flags() {
        // Issue #1050: a plain markdown body with no `---` block resolves all
        // metadata from flags (MCP `jira_create` parity).
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.md");
        fs::write(&file_path, "Just a plain markdown body.\n").unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: Some("PROJ".to_string()),
            r#type: Some("Story".to_string()),
            summary: Some("Flag-driven".to_string()),
            set_fields: vec![],
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.project, "PROJ");
        assert_eq!(params.issue_type, "Story");
        assert_eq!(params.summary, "Flag-driven");
    }

    #[test]
    fn resolve_from_jfm_no_frontmatter_defaults_issue_type_to_task() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.md");
        fs::write(&file_path, "Body.\n").unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: Some("PROJ".to_string()),
            r#type: None,
            summary: Some("S".to_string()),
            set_fields: vec![],
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.issue_type, "Task");
    }

    #[test]
    fn resolve_from_jfm_no_frontmatter_missing_project_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.md");
        fs::write(&file_path, "Body only.\n").unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: None,
            r#type: None,
            summary: Some("Has summary".to_string()),
            set_fields: vec![],
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("Project key is required"));
    }

    #[test]
    fn resolve_from_jfm_no_frontmatter_missing_summary_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.md");
        fs::write(&file_path, "Body only.\n").unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: Some("PROJ".to_string()),
            r#type: None,
            summary: None,
            set_fields: vec![],
            dry_run: false,
        };

        let err = cmd.resolve_params().unwrap_err();
        assert!(err.to_string().contains("Summary is required"));
    }

    #[test]
    fn resolve_from_jfm_partial_frontmatter_without_instance_falls_back_to_flags() {
        // Issue #1050: frontmatter present but missing `instance` and
        // `summary` — previously a hard parse error. Now `instance` is
        // irrelevant to create and `summary` falls back to the flag, while
        // frontmatter-only fields (project, labels) are still honored.
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\nproject: PROJ\nlabels:\n  - backend\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: None,
            project: None,
            r#type: None,
            summary: Some("From flag".to_string()),
            set_fields: vec![],
            dry_run: false,
        };

        let params = cmd.resolve_params().unwrap();
        assert_eq!(params.project, "PROJ");
        assert_eq!(params.summary, "From flag");
        assert_eq!(params.labels, vec!["backend"]);
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
            instance: None,
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
            instance: None,
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
            instance: None,
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
            adf: ValidatedAdfDocument::empty(),
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
    async fn execute_routes_create_to_instance_flag_url() {
        // Issue #1051: --instance routes the create request to the supplied
        // URL end-to-end. Covers execute()'s non-dry-run path (flag → client →
        // run_create) and proves the override actually targets that instance.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "100", "key": "PROJ-100", "self": "https://x/rest/api/3/issue/100"
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("body.md");
        fs::write(&file_path, "Plain body.\n").unwrap();

        // Email/token from env; instance comes from the --instance flag, which
        // points at the mock server.
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_EMAIL, "u@test.com");
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN, "token");

        let cmd = CreateCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            instance: Some(server.uri()),
            project: Some("PROJ".to_string()),
            r#type: None,
            summary: Some("From flag".to_string()),
            set_fields: vec![],
            dry_run: false,
        };

        cmd.execute().await.unwrap();
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
            instance: None,
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
            instance: None,
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
            instance: None,
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
