//! CLI command for writing content to JIRA issues.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::custom_fields::{
    apply_user_field_overrides, merge_set_field_overrides, parse_set_field, resolve_custom_fields,
};
use crate::atlassian::document::{CustomFieldSection, JfmDocument};
use crate::atlassian::jira_api::JiraApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{
    create_client, prepare_write, print_jira_dry_run_with_custom_fields, read_input, run_write,
    run_write_jira_with_resolved_fields,
};

/// Pushes content to a JIRA issue.
#[derive(Parser)]
pub struct WriteCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Input file (reads from stdin if omitted or "-"). Pass `--no-content`,
    /// or omit when `--assignee`/`--reporter`/`--set-field` is supplied
    /// alone, to leave the description untouched.
    pub file: Option<String>,

    /// Input format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Skips reading the description and leaves it untouched. Use with
    /// `--assignee`, `--reporter`, or `--set-field` to update other fields
    /// without rewriting the body.
    #[arg(long, conflicts_with = "file")]
    pub no_content: bool,

    /// Sets the assignee `accountId`. The empty string `""` clears the
    /// assignee; `"-1"` triggers JIRA automatic assignment. Use
    /// `omni-dev atlassian jira user search` to resolve a name or email
    /// to an `accountId`.
    #[arg(long, value_name = "ACCOUNT_ID")]
    pub assignee: Option<String>,

    /// Sets the reporter `accountId`. Same conventions as `--assignee`.
    #[arg(long, value_name = "ACCOUNT_ID")]
    pub reporter: Option<String>,

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
    /// Reads input (if any) and pushes the supplied changes to the JIRA
    /// issue. Description, title, assignee, reporter, and `--set-field`
    /// custom fields can all be updated independently; at least one must
    /// be supplied.
    pub async fn execute(self) -> Result<()> {
        // Real-run paths fetch a client from settings; tests substitute a
        // wiremock-backed one via `execute_with_client`.
        self.dispatch(|| create_client().map(|(c, _)| c)).await
    }

    /// Dispatches the write, fetching a client lazily via `make_client` only
    /// when the chosen branch needs to talk to the API. Dry-run and
    /// validation-failure branches return without invoking it.
    async fn dispatch<F>(self, make_client: F) -> Result<()>
    where
        F: FnOnce() -> Result<AtlassianClient>,
    {
        let overrides = self
            .set_fields
            .iter()
            .map(|s| parse_set_field(s))
            .collect::<Result<Vec<_>>>()?;

        let user_fields_present = self.assignee.is_some() || self.reporter.is_some();
        let other_fields_present = user_fields_present || !overrides.is_empty();

        if self.no_content && !other_fields_present {
            anyhow::bail!(
                "nothing to update: pass --assignee, --reporter, or --set-field, \
                 or remove --no-content to update the description"
            );
        }

        if matches!(self.format, ContentFormat::Adf) && !overrides.is_empty() {
            anyhow::bail!(
                "--set-field is only supported with --format jfm; ADF writes take a raw payload"
            );
        }

        // Read body / title / frontmatter scalars / custom-field sections.
        // Skip body parsing when --no-content is explicit, OR when the user
        // supplied no file *and* one of the field-update flags is set
        // (assignee/reporter/--set-field) — that combination signals a
        // "fields-only" update and should not block on stdin.
        let skip_body = self.no_content || (self.file.is_none() && other_fields_present);
        let (body_adf, title, frontmatter_scalars, sections): (
            Option<AdfDocument>,
            String,
            std::collections::BTreeMap<String, serde_yaml::Value>,
            Vec<CustomFieldSection>,
        ) = if skip_body {
            (
                None,
                String::new(),
                std::collections::BTreeMap::new(),
                vec![],
            )
        } else if matches!(self.format, ContentFormat::Adf) {
            let (adf, title) = prepare_write(self.file.as_deref(), &self.format)?;
            (Some(adf), title, std::collections::BTreeMap::new(), vec![])
        } else {
            let input = read_input(self.file.as_deref())?;
            let doc = JfmDocument::parse(&input)?;
            let (body_md, sections) = doc.split_custom_sections();
            let frontmatter_scalars = doc
                .frontmatter
                .jira_custom_fields()
                .cloned()
                .unwrap_or_default();
            let body_adf = markdown_to_adf(&body_md)?;
            let title = doc.frontmatter.title().to_string();
            (Some(body_adf), title, frontmatter_scalars, sections)
        };

        let scalars = merge_set_field_overrides(frontmatter_scalars, overrides);

        if self.dry_run {
            return print_jira_dry_run_with_custom_fields(
                &self.key,
                body_adf.as_ref(),
                &title,
                self.assignee.as_deref(),
                self.reporter.as_deref(),
                &scalars,
                &sections,
            );
        }

        let client = make_client()?;

        // Fast path: simple description+title update with no other field changes.
        if !user_fields_present && scalars.is_empty() && sections.is_empty() {
            // SAFETY: `body_adf` is always Some here because the
            // skip_body && !other_fields_present case was rejected above.
            let Some(body_adf) = body_adf else {
                unreachable!("skip_body without other fields was rejected above");
            };
            let validated = ValidatedAdfDocument::try_new(body_adf)?;
            let api = JiraApi::new(client);
            return run_write(&self.key, &validated, &title, self.force, &api).await;
        }

        // Resolve frontmatter / set-field custom fields via editmeta.
        let mut resolved = if !scalars.is_empty() || !sections.is_empty() {
            let editmeta = client.get_editmeta(&self.key).await?;
            resolve_custom_fields(&scalars, &sections, &editmeta)?
        } else {
            std::collections::BTreeMap::new()
        };

        // Layer typed user-field knobs on top, rejecting collisions with
        // anything already resolved into the same JIRA field id.
        apply_user_field_overrides(
            &mut resolved,
            self.assignee.as_deref(),
            self.reporter.as_deref(),
            "`--set-field` of the same name",
        )?;

        let validated_body = body_adf.map(ValidatedAdfDocument::try_new).transpose()?;
        run_write_jira_with_resolved_fields(
            &self.key,
            validated_body.as_ref(),
            &title,
            self.force,
            &resolved,
            &client,
        )
        .await
    }

    #[cfg(test)]
    async fn execute_with_client(self, client: AtlassianClient) -> Result<()> {
        self.dispatch(move || Ok(client)).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    fn cmd(key: &str) -> WriteCommand {
        WriteCommand {
            key: key.to_string(),
            file: None,
            format: ContentFormat::Jfm,
            no_content: false,
            assignee: None,
            reporter: None,
            set_fields: vec![],
            force: true,
            dry_run: false,
        }
    }

    #[test]
    fn write_command_struct_fields() {
        let mut c = cmd("PROJ-1");
        c.file = Some("input.md".to_string());
        assert_eq!(c.key, "PROJ-1");
        assert!(c.force);
        assert!(!c.dry_run);
    }

    #[test]
    fn dry_run_does_not_call_api() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Test\n---\n\nBody content\n";
        fs::write(&file_path, content).unwrap();

        let mut c = cmd("PROJ-1");
        c.file = Some(file_path.to_str().unwrap().to_string());
        c.force = false;
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(c.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn set_field_with_adf_format_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.json");
        fs::write(&file_path, r#"{"version":1,"type":"doc","content":[]}"#).unwrap();

        let mut c = cmd("PROJ-1");
        c.file = Some(file_path.to_str().unwrap().to_string());
        c.format = ContentFormat::Adf;
        c.set_fields = vec!["Priority=High".to_string()];
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.execute()).unwrap_err();
        assert!(err
            .to_string()
            .contains("--set-field is only supported with --format jfm"));
    }

    #[test]
    fn invalid_set_field_syntax_errors() {
        let mut c = cmd("PROJ-1");
        c.set_fields = vec!["no-equals-sign".to_string()];
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.execute()).unwrap_err();
        assert!(err.to_string().contains("expected --set-field"));
    }

    #[test]
    fn dry_run_with_set_field_prints_custom_fields() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content =
            "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: T\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let mut c = cmd("PROJ-1");
        c.file = Some(file_path.to_str().unwrap().to_string());
        c.set_fields = vec!["Priority=High".to_string()];
        c.force = false;
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(c.execute()).is_ok());
    }

    #[test]
    fn dry_run_adf_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let mut c = cmd("PROJ-1");
        c.file = Some(file_path.to_str().unwrap().to_string());
        c.format = ContentFormat::Adf;
        c.force = false;
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(c.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn no_content_without_other_changes_errors() {
        let mut c = cmd("PROJ-1");
        c.no_content = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.execute()).unwrap_err();
        assert!(err.to_string().contains("nothing to update"));
    }

    #[test]
    fn dry_run_no_content_with_assignee() {
        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.assignee = Some("abc123".to_string());
        c.force = false;
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(c.execute()).is_ok());
    }

    #[test]
    fn dry_run_no_content_with_empty_assignee_unassigns() {
        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.assignee = Some(String::new());
        c.force = false;
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(c.execute()).is_ok());
    }

    #[test]
    fn dry_run_no_content_with_reporter() {
        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.reporter = Some("rep123".to_string());
        c.force = false;
        c.dry_run = true;

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(c.execute()).is_ok());
    }

    // ── execute_with_client (real-write paths) ────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    fn write_adf_file(dir: &tempfile::TempDir, body: &str) -> String {
        let p = dir.path().join("issue.json");
        let json = format!(
            r#"{{"version":1,"type":"doc","content":[{{"type":"paragraph","content":[{{"type":"text","text":"{body}"}}]}}]}}"#
        );
        fs::write(&p, json).unwrap();
        p.to_str().unwrap().to_string()
    }

    fn write_jfm_file(dir: &tempfile::TempDir, body: &str) -> String {
        let p = dir.path().join("issue.md");
        let content = format!(
            "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: T\n---\n\n{body}\n"
        );
        fs::write(&p, content).unwrap();
        p.to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn execute_adf_body_no_parent_sends_description_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_adf_file(&dir, "Hello");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.file = Some(path);
        c.format = ContentFormat::Adf;
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_send_path_rejects_invalid_adf_nesting() {
        // Issue #714: when --dry-run is NOT set, the dispatch wraps body_adf
        // in `ValidatedAdfDocument::try_new(...)?` before reaching the API.
        // A body that produces invalid ADF (panel→expand) must short-circuit
        // with a validation error before any HTTP call.
        let dir = tempfile::tempdir().unwrap();
        let path = write_jfm_file(
            &dir,
            ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::",
        );

        let server = wiremock::MockServer::start().await;
        // Intentionally no PUT mock — validation must short-circuit first.

        let mut c = cmd("PROJ-1");
        c.file = Some(path);
        let err = c
            .execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }

    #[tokio::test]
    async fn execute_jfm_body_no_parent_no_fields_uses_run_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jfm_file(&dir, "Body");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.file = Some(path);
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    // ── execute_with_client tests for assignee / reporter / fields ────

    #[tokio::test]
    async fn execute_no_content_with_assignee_only_sends_put_with_assignee() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"assignee": {"accountId": "abc123"}}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.assignee = Some("abc123".to_string());
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_no_content_with_empty_assignee_clears_via_null_payload() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"assignee": null}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.assignee = Some(String::new());
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_no_content_with_reporter_sends_put_with_reporter() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"reporter": {"accountId": "rep123"}}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.reporter = Some("rep123".to_string());
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_jfm_body_with_assignee_sends_combined_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jfm_file(&dir, "Body line");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.file = Some(path);
        c.assignee = Some("abc123".to_string());
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    // ── --set-field for rich-text custom fields (issue #866) ──────────

    async fn mount_textarea_editmeta(server: &wiremock::MockServer, key: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/rest/api/3/issue/{key}/editmeta"
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "fields": {
                        "customfield_19300": {
                            "name": "Acceptance Criteria",
                            "schema": {
                                "type": "string",
                                "custom": "com.atlassian.jira.plugin.system.customfieldtypes:textarea"
                            }
                        }
                    }
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn execute_set_field_textarea_string_converts_jfm_to_adf() {
        // Issue #866: --set-field NAME=VALUE where VALUE is a JFM string and
        // NAME is a rich-text custom field should auto-convert to ADF rather
        // than rejecting.
        let server = wiremock::MockServer::start().await;
        mount_textarea_editmeta(&server, "PROJ-1").await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "customfield_19300": {
                        "version": 1,
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "content": [{"type": "text", "text": "hello world"}]
                        }]
                    }
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.set_fields = vec!["Acceptance Criteria=hello world".to_string()];
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_set_field_textarea_empty_string_clears() {
        let server = wiremock::MockServer::start().await;
        mount_textarea_editmeta(&server, "PROJ-1").await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"customfield_19300": null}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.set_fields = vec!["Acceptance Criteria=".to_string()];
        c.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_set_field_textarea_non_string_errors() {
        // `--set-field "Acceptance Criteria=42"` parses to a number. Rich-text
        // fields don't accept non-string scalars — error message should point
        // at the rich-text + JFM contract.
        let server = wiremock::MockServer::start().await;
        mount_textarea_editmeta(&server, "PROJ-1").await;

        let mut c = cmd("PROJ-1");
        c.no_content = true;
        c.set_fields = vec!["Acceptance Criteria=42".to_string()];
        let err = c
            .execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rich-text field"), "got: {msg}");
        assert!(msg.contains("JFM markdown"), "got: {msg}");
    }

}
