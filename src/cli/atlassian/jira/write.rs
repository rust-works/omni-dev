//! CLI command for writing content to JIRA issues.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::AtlassianClient;
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::custom_fields::{
    merge_set_field_overrides, parse_set_field, resolve_custom_fields,
};
use crate::atlassian::document::{validate_issue_key, JfmDocument};
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

    /// Input file (reads from stdin if omitted or "-"). Optional when
    /// `--parent` is supplied alone — the description is left untouched.
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

    /// Sets the parent issue key (e.g., establishes Epic → Story or
    /// Story → Sub-task hierarchy). Maps to JIRA's `parent` system field;
    /// distinct from "Composition" links created via `omni-dev jira link`.
    #[arg(long, value_name = "KEY")]
    pub parent: Option<String>,

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

        if let Some(ref parent_key) = self.parent {
            validate_issue_key(parent_key)?;
        }
        let parent = self.parent.as_deref();

        if matches!(self.format, ContentFormat::Adf) {
            if !overrides.is_empty() {
                anyhow::bail!(
                    "--set-field is only supported with --format jfm; ADF writes take a raw payload"
                );
            }
            // ADF + --parent + body: send both. ADF + --parent alone: parent-only.
            if self.file.is_none() && parent.is_some() {
                if self.dry_run {
                    return print_jira_dry_run_with_custom_fields(
                        &self.key,
                        None,
                        "",
                        parent,
                        &std::collections::BTreeMap::new(),
                        &[],
                    );
                }
                let client = make_client()?;
                return run_write_jira_with_resolved_fields(
                    &self.key,
                    None,
                    "",
                    parent,
                    self.force,
                    &std::collections::BTreeMap::new(),
                    &client,
                )
                .await;
            }
            let (adf, title) = prepare_write(self.file.as_deref(), &self.format)?;
            if self.dry_run {
                if parent.is_some() {
                    return print_jira_dry_run_with_custom_fields(
                        &self.key,
                        Some(&adf),
                        &title,
                        parent,
                        &std::collections::BTreeMap::new(),
                        &[],
                    );
                }
                return print_dry_run(&self.key, &adf, &title);
            }
            let client = make_client()?;
            if parent.is_some() {
                return run_write_jira_with_resolved_fields(
                    &self.key,
                    Some(&adf),
                    &title,
                    parent,
                    self.force,
                    &std::collections::BTreeMap::new(),
                    &client,
                )
                .await;
            }
            let api = JiraApi::new(client);
            return run_write(&self.key, &adf, &title, self.force, &api).await;
        }

        // JFM path: may carry custom fields in frontmatter or body sections.
        // Parent-only update (no file, no stdin): skip JFM parsing entirely.
        if self.file.is_none() && parent.is_some() && overrides.is_empty() {
            if self.dry_run {
                return print_jira_dry_run_with_custom_fields(
                    &self.key,
                    None,
                    "",
                    parent,
                    &std::collections::BTreeMap::new(),
                    &[],
                );
            }
            let client = make_client()?;
            return run_write_jira_with_resolved_fields(
                &self.key,
                None,
                "",
                parent,
                self.force,
                &std::collections::BTreeMap::new(),
                &client,
            )
            .await;
        }

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
                &self.key,
                Some(&body_adf),
                &title,
                parent,
                &scalars,
                &sections,
            );
        }

        let client = make_client()?;

        if scalars.is_empty() && sections.is_empty() && parent.is_none() {
            let api = JiraApi::new(client);
            return run_write(&self.key, &body_adf, &title, self.force, &api).await;
        }

        let resolved = if scalars.is_empty() && sections.is_empty() {
            std::collections::BTreeMap::new()
        } else {
            let editmeta = client.get_editmeta(&self.key).await?;
            resolve_custom_fields(&scalars, &sections, &editmeta)?
        };

        run_write_jira_with_resolved_fields(
            &self.key,
            Some(&body_adf),
            &title,
            parent,
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

    #[test]
    fn write_command_struct_fields() {
        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some("input.md".to_string()),
            format: ContentFormat::Jfm,
            set_fields: vec![],
            parent: None,
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
            parent: None,
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
            parent: None,
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
            parent: None,
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
            parent: None,
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
            parent: None,
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn dry_run_parent_only_skips_description() {
        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: None,
            format: ContentFormat::Jfm,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute());
        assert!(result.is_ok());
    }

    #[test]
    fn invalid_parent_key_errors() {
        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: None,
            format: ContentFormat::Jfm,
            set_fields: vec![],
            parent: Some("not a key".to_string()),
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(cmd.execute()).unwrap_err();
        assert!(err.to_string().contains("Invalid JIRA issue key"));
    }

    #[test]
    fn dry_run_body_with_parent_prints_both() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: T\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(cmd.execute()).is_ok());
    }

    #[test]
    fn dry_run_adf_parent_only_skips_description() {
        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: None,
            format: ContentFormat::Adf,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(cmd.execute()).is_ok());
    }

    #[test]
    fn dry_run_adf_body_with_parent_prints_both() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hi"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(file_path.to_str().unwrap().to_string()),
            format: ContentFormat::Adf,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: false,
            dry_run: true,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(cmd.execute()).is_ok());
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
    async fn execute_adf_parent_only_sends_parent_field() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"parent": {"key": "PROJ-99"}}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: None,
            format: ContentFormat::Adf,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: true,
            dry_run: false,
        };
        cmd.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_adf_body_with_parent_sends_both() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_adf_file(&dir, "Hello");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "description": {
                        "version": 1,
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "content": [{"type": "text", "text": "Hello"}]
                        }]
                    },
                    "parent": {"key": "PROJ-99"}
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(path),
            format: ContentFormat::Adf,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: true,
            dry_run: false,
        };
        cmd.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
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

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(path),
            format: ContentFormat::Adf,
            set_fields: vec![],
            parent: None,
            force: true,
            dry_run: false,
        };
        cmd.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_jfm_parent_only_sends_parent_field() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {"parent": {"key": "PROJ-99"}}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: None,
            format: ContentFormat::Jfm,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: true,
            dry_run: false,
        };
        cmd.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
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

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(path),
            format: ContentFormat::Jfm,
            set_fields: vec![],
            parent: None,
            force: true,
            dry_run: false,
        };
        cmd.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn execute_jfm_body_with_parent_sends_both() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jfm_file(&dir, "Body");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "description": {
                        "version": 1,
                        "type": "doc",
                        "content": [{
                            "type": "paragraph",
                            "content": [{"type": "text", "text": "Body"}]
                        }]
                    },
                    "summary": "T",
                    "parent": {"key": "PROJ-99"}
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = WriteCommand {
            key: "PROJ-1".to_string(),
            file: Some(path),
            format: ContentFormat::Jfm,
            set_fields: vec![],
            parent: Some("PROJ-99".to_string()),
            force: true,
            dry_run: false,
        };
        cmd.execute_with_client(mock_client(&server.uri()))
            .await
            .unwrap();
    }
}
