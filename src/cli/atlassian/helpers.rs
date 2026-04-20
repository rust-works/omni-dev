//! Shared helpers for JIRA and Confluence CLI commands.

use std::fs;
use std::io::{self, Read, Write};

use anyhow::{Context, Result};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::api::AtlassianApi;
use crate::atlassian::auth;
use crate::atlassian::client::{AtlassianClient, FieldSelection};
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::document::{content_item_to_document, issue_to_jfm_document, JfmDocument};

use super::format::ContentFormat;

/// Fetches content and outputs it in the specified format.
pub async fn run_read(
    id: &str,
    output: Option<&str>,
    format: &ContentFormat,
    api: &dyn AtlassianApi,
    instance_url: &str,
) -> Result<()> {
    let item = api.get_content(id).await?;

    match format {
        ContentFormat::Adf => {
            let json =
                serde_json::to_string_pretty(&item.body_adf.unwrap_or(serde_json::Value::Null))
                    .context("Failed to serialize ADF JSON")?;
            output_text(&json, output)?;
        }
        ContentFormat::Jfm => {
            let doc = content_item_to_document(&item, instance_url)?;
            let rendered = doc.render()?;
            output_text(&rendered, output)?;

            if let Some(path) = output {
                eprintln!("Saved to: {path}");
            }
        }
    }

    Ok(())
}

/// Fetches a JIRA issue with the given field selection and outputs it.
///
/// JIRA-specific path used when `--fields` or `--all-fields` is set. Calls
/// [`AtlassianClient::get_issue_with_fields`] directly rather than the
/// generic [`AtlassianApi`] trait, since `ContentItem` does not carry
/// custom field data.
pub async fn run_read_jira_with_fields(
    key: &str,
    output: Option<&str>,
    format: &ContentFormat,
    selection: FieldSelection,
    client: &AtlassianClient,
    instance_url: &str,
) -> Result<()> {
    let issue = client.get_issue_with_fields(key, selection).await?;

    match format {
        ContentFormat::Adf => {
            let mut fields = serde_json::Map::new();
            if let Some(desc) = &issue.description_adf {
                fields.insert("description".to_string(), desc.clone());
            }
            for cf in &issue.custom_fields {
                fields.insert(cf.id.clone(), cf.value.clone());
            }
            let json = serde_json::to_string_pretty(&serde_json::Value::Object(fields))
                .context("Failed to serialize fields as JSON")?;
            output_text(&json, output)?;
        }
        ContentFormat::Jfm => {
            let doc = issue_to_jfm_document(&issue, instance_url)?;
            let rendered = doc.render()?;
            output_text(&rendered, output)?;

            if let Some(path) = output {
                eprintln!("Saved to: {path}");
            }
        }
    }

    Ok(())
}

/// Parses input content and converts it to ADF, returning the document and title.
pub fn prepare_write(file: Option<&str>, format: &ContentFormat) -> Result<(AdfDocument, String)> {
    let input = read_input(file)?;

    match format {
        ContentFormat::Jfm => {
            let doc = JfmDocument::parse(&input)?;
            let adf = markdown_to_adf(&doc.body)?;
            let title = doc.frontmatter.title().to_string();
            Ok((adf, title))
        }
        ContentFormat::Adf => {
            let adf = AdfDocument::from_json_str(&input)?;
            Ok((adf, String::new()))
        }
    }
}

/// Prints a dry-run summary without making any API calls.
pub fn print_dry_run(id: &str, adf: &AdfDocument, title: &str) -> Result<()> {
    println!("Dry run for {id}:");
    if !title.is_empty() {
        println!("  Title: {title}");
    }
    println!(
        "\nADF output:\n{}",
        serde_json::to_string_pretty(adf).context("Failed to serialize ADF")?
    );
    Ok(())
}

/// Prints a dry-run summary for issue creation.
pub fn print_create_dry_run(
    project: &str,
    issue_type: &str,
    summary: &str,
    adf: &AdfDocument,
    labels: &[String],
) -> Result<()> {
    println!("Dry run — would create issue:");
    println!("  Project:    {project}");
    println!("  Type:       {issue_type}");
    println!("  Summary:    {summary}");
    if !labels.is_empty() {
        println!("  Labels:     {}", labels.join(", "));
    }
    println!(
        "\nADF body:\n{}",
        serde_json::to_string_pretty(adf).context("Failed to serialize ADF")?
    );
    Ok(())
}

/// Confirms and pushes content to the target.
pub async fn run_write(
    id: &str,
    adf: &AdfDocument,
    title: &str,
    force: bool,
    api: &dyn AtlassianApi,
) -> Result<()> {
    if !force {
        println!("About to update {id}:");
        if !title.is_empty() {
            println!("  Title: {title}");
        }
        print!("\nApply changes? [y/N] ");
        io::stdout().flush()?;

        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let title_ref = if title.is_empty() { None } else { Some(title) };

    api.update_content(id, adf, title_ref).await?;
    println!("Updated {id} successfully.");

    Ok(())
}

/// Confirms and pushes content (description, title, and custom fields) to
/// a JIRA issue via [`AtlassianClient::update_issue_with_custom_fields`].
///
/// Used by the JIRA write path when custom fields are present. Goes direct
/// to the client rather than through [`AtlassianApi`] since the trait does
/// not model custom fields.
pub async fn run_write_jira_with_resolved_fields(
    key: &str,
    adf: &AdfDocument,
    title: &str,
    force: bool,
    custom_fields: &std::collections::BTreeMap<String, serde_json::Value>,
    client: &AtlassianClient,
) -> Result<()> {
    if !force {
        println!("About to update {key}:");
        if !title.is_empty() {
            println!("  Title: {title}");
        }
        if !custom_fields.is_empty() {
            println!("  Custom fields: {}", custom_fields.len());
        }
        print!("\nApply changes? [y/N] ");
        io::stdout().flush()?;

        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let title_ref = if title.is_empty() { None } else { Some(title) };

    client
        .update_issue_with_custom_fields(key, adf, title_ref, custom_fields)
        .await?;
    println!("Updated {key} successfully.");

    Ok(())
}

/// Prints a dry-run summary including the resolved custom fields payload.
pub fn print_jira_dry_run_with_custom_fields(
    key: &str,
    adf: &AdfDocument,
    title: &str,
    scalars: &std::collections::BTreeMap<String, serde_yaml::Value>,
    sections: &[crate::atlassian::document::CustomFieldSection],
) -> Result<()> {
    print_dry_run(key, adf, title)?;
    if !scalars.is_empty() {
        println!("\nCustom field scalars (frontmatter):");
        for (name, value) in scalars {
            let rendered =
                serde_yaml::to_string(value).context("Failed to serialize scalar as YAML")?;
            println!("  {name}: {}", rendered.trim());
        }
    }
    if !sections.is_empty() {
        println!("\nCustom field sections (body):");
        for section in sections {
            println!("  - {} ({})", section.name, section.id);
        }
    }
    Ok(())
}

/// Interactive fetch-edit-push cycle.
pub async fn run_edit(id: &str, api: &dyn AtlassianApi, instance_url: &str) -> Result<()> {
    use tracing::debug;

    // 1. Fetch the content
    println!("Fetching {id}...");
    let item = api.get_content(id).await?;
    let original_title = item.title.clone();

    // 2. Convert to JFM document
    let doc = content_item_to_document(&item, instance_url)?;
    let original_content = doc.render()?;

    // 3. Write to temp file
    let temp_dir = tempfile::tempdir()?;
    let temp_file = temp_dir.path().join(format!("{id}.md"));
    fs::write(&temp_file, &original_content)?;

    println!("Saved to: {}", temp_file.display());

    // 4. Interactive loop
    loop {
        print!("\n[A]ccept, [S]how, [E]dit, or [Q]uit? [a/s/e/q] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim().to_lowercase().as_str() {
            "a" | "accept" => {
                let final_content =
                    fs::read_to_string(&temp_file).context("Failed to read temp file")?;

                if final_content == original_content {
                    println!("No changes detected.");
                    return Ok(());
                }

                let final_doc = JfmDocument::parse(&final_content)?;
                debug!(
                    "Parsed JFM document, body length: {} bytes",
                    final_doc.body.len()
                );

                let adf = markdown_to_adf(&final_doc.body)?;
                debug!(
                    "Converted to ADF with {} top-level nodes",
                    adf.content.len()
                );
                if tracing::enabled!(tracing::Level::TRACE) {
                    let adf_json = serde_json::to_string_pretty(&adf)
                        .unwrap_or_else(|e| format!("<serialization error: {e}>"));
                    tracing::trace!("ADF payload:\n{adf_json}");
                }

                let title_changed = final_doc.frontmatter.title() != original_title;
                let title_update = if title_changed {
                    Some(final_doc.frontmatter.title())
                } else {
                    None
                };

                api.update_content(id, &adf, title_update).await?;
                println!("Updated {id} successfully.");
                return Ok(());
            }
            "s" | "show" => {
                let content = fs::read_to_string(&temp_file).context("Failed to read temp file")?;
                println!("\n{content}");
            }
            "e" | "edit" => {
                open_editor(&temp_file)?;
            }
            "q" | "quit" => {
                println!("Cancelled.");
                return Ok(());
            }
            _ => {
                println!(
                    "Invalid choice. Enter 'a' to accept, 's' to show, 'e' to edit, or 'q' to quit."
                );
            }
        }
    }
}

/// Creates an authenticated Atlassian API client, returning the client and instance URL.
pub fn create_client() -> Result<(AtlassianClient, String)> {
    let credentials = auth::load_credentials()?;
    let client = AtlassianClient::from_credentials(&credentials)?;
    let instance_url = client.instance_url().to_string();
    Ok((client, instance_url))
}

/// Writes text to a file or stdout.
pub fn output_text(text: &str, file: Option<&str>) -> Result<()> {
    match file {
        Some(path) => {
            fs::write(path, text).with_context(|| format!("Failed to write to {path}"))?;
        }
        None => {
            print!("{text}");
        }
    }
    Ok(())
}

/// Reads input from a file path or stdin.
pub fn read_input(file: Option<&str>) -> Result<String> {
    match file {
        Some("-") | None => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("Failed to read from stdin")?;
            Ok(buf)
        }
        Some(path) => {
            fs::read_to_string(path).with_context(|| format!("Failed to read file: {path}"))
        }
    }
}

/// Opens a file in the user's editor.
fn open_editor(file: &std::path::Path) -> Result<()> {
    use std::env;
    use std::process::Command;

    let editor = if let Ok(e) = env::var("OMNI_DEV_EDITOR").or_else(|_| env::var("EDITOR")) {
        e
    } else {
        print!("Neither OMNI_DEV_EDITOR nor EDITOR is set. Enter editor command: ");
        io::stdout().flush().context("Failed to flush stdout")?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read user input")?;
        input.trim().to_string()
    };

    if editor.is_empty() {
        println!("No editor specified. Returning to menu.");
        return Ok(());
    }

    let (editor_cmd, args) = crate::cli::git::formatting::parse_editor_command(&editor);

    let mut command = Command::new(editor_cmd);
    command.args(args);
    command.arg(file.to_string_lossy().as_ref());

    match command.status() {
        Ok(status) => {
            if status.success() {
                println!("Editor session completed.");
            } else {
                println!("Editor exited with non-zero status: {:?}", status.code());
            }
        }
        Err(e) => {
            println!("Failed to execute editor '{editor}': {e}");
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::api::{ContentItem, ContentMetadata};

    // ── Mock AtlassianApi ──────────────────────────────────────────

    struct MockApi {
        content: ContentItem,
        update_called: std::sync::Mutex<bool>,
    }

    impl MockApi {
        fn jira_issue(body_adf: Option<serde_json::Value>) -> Self {
            Self {
                content: ContentItem {
                    id: "PROJ-1".to_string(),
                    title: "Test Issue".to_string(),
                    body_adf,
                    metadata: ContentMetadata::Jira {
                        status: Some("Open".to_string()),
                        issue_type: Some("Bug".to_string()),
                        assignee: None,
                        priority: None,
                        labels: vec![],
                    },
                },
                update_called: std::sync::Mutex::new(false),
            }
        }

        fn confluence_page(body_adf: Option<serde_json::Value>) -> Self {
            Self {
                content: ContentItem {
                    id: "12345".to_string(),
                    title: "Test Page".to_string(),
                    body_adf,
                    metadata: ContentMetadata::Confluence {
                        space_key: "ENG".to_string(),
                        status: Some("current".to_string()),
                        version: Some(1),
                        parent_id: None,
                    },
                },
                update_called: std::sync::Mutex::new(false),
            }
        }

        fn was_update_called(&self) -> bool {
            *self.update_called.lock().unwrap()
        }
    }

    impl AtlassianApi for MockApi {
        fn get_content<'a>(
            &'a self,
            _id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ContentItem>> + Send + 'a>>
        {
            Box::pin(async { Ok(self.content.clone()) })
        }

        fn update_content<'a>(
            &'a self,
            _id: &'a str,
            _body_adf: &'a AdfDocument,
            _title: Option<&'a str>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            *self.update_called.lock().unwrap() = true;
            Box::pin(async { Ok(()) })
        }

        fn verify_auth<'a>(
            &'a self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
        {
            Box::pin(async { Ok("Test User".to_string()) })
        }

        fn backend_name(&self) -> &'static str {
            "mock"
        }
    }

    // ── output_text ────────────────────────────────────────────────

    #[test]
    fn output_text_to_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("output.txt");
        let path_str = file_path.to_str().unwrap();

        output_text("hello world", Some(path_str)).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn output_text_to_stdout() {
        assert!(output_text("test", None).is_ok());
    }

    #[test]
    fn output_text_overwrites_existing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("output.txt");
        let path_str = file_path.to_str().unwrap();

        fs::write(&file_path, "old content").unwrap();
        output_text("new content", Some(path_str)).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "new content");
    }

    #[test]
    fn output_text_invalid_path() {
        let result = output_text("data", Some("/nonexistent_dir/file.txt"));
        assert!(result.is_err());
    }

    // ── read_input ─────────────────────────────────────────────────

    #[test]
    fn read_input_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: Test\n---\n\nBody\n";
        fs::write(&file_path, content).unwrap();

        let result = read_input(Some(file_path.to_str().unwrap())).unwrap();
        assert_eq!(result, content);
    }

    #[test]
    fn read_input_missing_file() {
        let result = read_input(Some("/nonexistent/file.md"));
        assert!(result.is_err());
    }

    // ── open_editor ────────────────────────────────────────────────

    #[test]
    fn open_editor_with_true_command() {
        std::env::set_var("OMNI_DEV_EDITOR", "true");

        let temp_dir = tempfile::tempdir().unwrap();
        let file = temp_dir.path().join("test.md");
        fs::write(&file, "content").unwrap();

        let result = open_editor(&file);

        std::env::remove_var("OMNI_DEV_EDITOR");

        assert!(result.is_ok());
    }

    #[test]
    fn open_editor_with_nonexistent_command() {
        std::env::set_var("OMNI_DEV_EDITOR", "nonexistent_editor_binary_12345");

        let temp_dir = tempfile::tempdir().unwrap();
        let file = temp_dir.path().join("test.md");
        fs::write(&file, "content").unwrap();

        let result = open_editor(&file);

        std::env::remove_var("OMNI_DEV_EDITOR");

        assert!(result.is_ok());
    }

    // ── prepare_write ──────────────────────────────────────────────

    #[test]
    fn prepare_write_jfm_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.md");
        let content = "---\ntype: jira\ninstance: https://org.atlassian.net\nkey: PROJ-1\nsummary: My Title\n---\n\nHello world\n";
        fs::write(&file_path, content).unwrap();

        let (adf, title) =
            prepare_write(Some(file_path.to_str().unwrap()), &ContentFormat::Jfm).unwrap();

        assert_eq!(title, "My Title");
        assert!(!adf.content.is_empty());
    }

    #[test]
    fn prepare_write_adf_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("issue.json");
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf_json).unwrap();

        let (adf, title) =
            prepare_write(Some(file_path.to_str().unwrap()), &ContentFormat::Adf).unwrap();

        assert!(title.is_empty());
        assert_eq!(adf.content.len(), 1);
    }

    #[test]
    fn prepare_write_invalid_adf_json() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("bad.json");
        fs::write(&file_path, "not json").unwrap();

        let result = prepare_write(Some(file_path.to_str().unwrap()), &ContentFormat::Adf);
        assert!(result.is_err());
    }

    #[test]
    fn prepare_write_null_adf_input_yields_empty_doc() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("null.json");
        fs::write(&file_path, "null").unwrap();

        let (adf, title) =
            prepare_write(Some(file_path.to_str().unwrap()), &ContentFormat::Adf).unwrap();

        assert_eq!(adf, AdfDocument::default());
        assert!(title.is_empty());
    }

    #[test]
    fn prepare_write_missing_file() {
        let result = prepare_write(Some("/nonexistent/file.md"), &ContentFormat::Jfm);
        assert!(result.is_err());
    }

    // ── print_dry_run ──────────────────────────────────────────────

    #[test]
    fn print_dry_run_with_title() {
        let adf = AdfDocument::new();
        let result = print_dry_run("PROJ-1", &adf, "My Title");
        assert!(result.is_ok());
    }

    #[test]
    fn print_dry_run_without_title() {
        let adf = AdfDocument::new();
        let result = print_dry_run("PROJ-1", &adf, "");
        assert!(result.is_ok());
    }

    // ── run_read ───────────────────────────────────────────────

    #[tokio::test]
    async fn run_read_jfm_to_stdout() {
        let adf_body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hello"}]}]
        });
        let api = MockApi::jira_issue(Some(adf_body));

        let result = run_read(
            "PROJ-1",
            None,
            &ContentFormat::Jfm,
            &api,
            "https://org.atlassian.net",
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_read_adf_to_stdout() {
        let adf_body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": []
        });
        let api = MockApi::jira_issue(Some(adf_body));

        let result = run_read(
            "PROJ-1",
            None,
            &ContentFormat::Adf,
            &api,
            "https://org.atlassian.net",
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_read_adf_null_body() {
        let api = MockApi::jira_issue(None);

        let result = run_read(
            "PROJ-1",
            None,
            &ContentFormat::Adf,
            &api,
            "https://org.atlassian.net",
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_read_jfm_to_file() {
        let adf_body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hello"}]}]
        });
        let api = MockApi::jira_issue(Some(adf_body));

        let temp_dir = tempfile::tempdir().unwrap();
        let out_path = temp_dir.path().join("out.md");

        let result = run_read(
            "PROJ-1",
            Some(out_path.to_str().unwrap()),
            &ContentFormat::Jfm,
            &api,
            "https://org.atlassian.net",
        )
        .await;
        assert!(result.is_ok());
        assert!(out_path.exists());
        let content = fs::read_to_string(&out_path).unwrap();
        assert!(content.contains("PROJ-1"));
    }

    #[tokio::test]
    async fn run_read_adf_to_file() {
        let adf_body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": []
        });
        let api = MockApi::jira_issue(Some(adf_body));

        let temp_dir = tempfile::tempdir().unwrap();
        let out_path = temp_dir.path().join("out.json");

        let result = run_read(
            "PROJ-1",
            Some(out_path.to_str().unwrap()),
            &ContentFormat::Adf,
            &api,
            "https://org.atlassian.net",
        )
        .await;
        assert!(result.is_ok());
        assert!(out_path.exists());
    }

    #[tokio::test]
    async fn run_read_confluence_jfm() {
        let adf_body = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Page content"}]}]
        });
        let api = MockApi::confluence_page(Some(adf_body));

        let result = run_read(
            "12345",
            None,
            &ContentFormat::Jfm,
            &api,
            "https://org.atlassian.net",
        )
        .await;
        assert!(result.is_ok());
    }

    // ── run_write ──────────────────────────────────────────────

    #[tokio::test]
    async fn run_write_force_with_title() {
        let api = MockApi::jira_issue(None);
        let adf = AdfDocument::new();

        let result = run_write("PROJ-1", &adf, "My Title", true, &api).await;
        assert!(result.is_ok());
        assert!(api.was_update_called());
    }

    #[tokio::test]
    async fn run_write_force_empty_title() {
        let api = MockApi::jira_issue(None);
        let adf = AdfDocument::new();

        let result = run_write("PROJ-1", &adf, "", true, &api).await;
        assert!(result.is_ok());
        assert!(api.was_update_called());
    }

    // ── print_create_dry_run ───────────────────────────────────────

    #[test]
    fn print_create_dry_run_with_labels() {
        let adf = AdfDocument::new();
        let labels = vec!["backend".to_string(), "urgent".to_string()];
        let result = print_create_dry_run("PROJ", "Bug", "Fix login", &adf, &labels);
        assert!(result.is_ok());
    }

    #[test]
    fn print_create_dry_run_without_labels() {
        let adf = AdfDocument::new();
        let result = print_create_dry_run("PROJ", "Task", "Add feature", &adf, &[]);
        assert!(result.is_ok());
    }

    // ── run_read_jira_with_fields ──────────────────────────────────

    async fn setup_jira_fields_mock() -> (wiremock::MockServer, AtlassianClient) {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "key": "ACCS-1",
                    "fields": {
                        "summary": "Custom field issue",
                        "description": {
                            "type": "doc",
                            "version": 1,
                            "content": [{
                                "type": "paragraph",
                                "content": [{"type": "text", "text": "Main description"}]
                            }]
                        },
                        "status": {"name": "Open"},
                        "issuetype": {"name": "Bug"},
                        "assignee": null,
                        "priority": null,
                        "labels": [],
                        "customfield_19300": {
                            "type": "doc",
                            "version": 1,
                            "content": [{
                                "type": "paragraph",
                                "content": [{"type": "text", "text": "Criterion body"}]
                            }]
                        },
                        "customfield_10001": {"value": "Unplanned"}
                    },
                    "names": {
                        "customfield_19300": "Acceptance Criteria",
                        "customfield_10001": "Planned / Unplanned Work"
                    }
                })),
            )
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        (server, client)
    }

    #[tokio::test]
    async fn run_read_jira_with_fields_jfm_emits_scalars_and_sections() {
        let (_server, client) = setup_jira_fields_mock().await;
        let instance_url = client.instance_url().to_string();

        let temp_dir = tempfile::tempdir().unwrap();
        let out_path = temp_dir.path().join("issue.md");

        run_read_jira_with_fields(
            "ACCS-1",
            Some(out_path.to_str().unwrap()),
            &ContentFormat::Jfm,
            FieldSelection::All,
            &client,
            &instance_url,
        )
        .await
        .unwrap();

        let rendered = fs::read_to_string(&out_path).unwrap();
        assert!(rendered.contains("key: ACCS-1"));
        assert!(rendered.contains("custom_fields:"));
        assert!(rendered.contains("Planned / Unplanned Work"));
        assert!(rendered.contains("Unplanned"));
        assert!(rendered.contains("Main description"));
        assert!(rendered.contains("<!-- field: Acceptance Criteria (customfield_19300) -->"));
        assert!(rendered.contains("Criterion body"));
    }

    #[tokio::test]
    async fn run_read_jira_with_fields_adf_emits_field_map_json() {
        let (_server, client) = setup_jira_fields_mock().await;
        let instance_url = client.instance_url().to_string();

        let temp_dir = tempfile::tempdir().unwrap();
        let out_path = temp_dir.path().join("issue.json");

        run_read_jira_with_fields(
            "ACCS-1",
            Some(out_path.to_str().unwrap()),
            &ContentFormat::Adf,
            FieldSelection::All,
            &client,
            &instance_url,
        )
        .await
        .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&out_path).unwrap()).unwrap();
        assert_eq!(json["description"]["type"], "doc");
        assert_eq!(json["customfield_19300"]["type"], "doc");
        assert_eq!(json["customfield_10001"]["value"], "Unplanned");
    }

    #[tokio::test]
    async fn run_read_jira_with_fields_jfm_to_stdout() {
        let (_server, client) = setup_jira_fields_mock().await;
        let instance_url = client.instance_url().to_string();

        let result = run_read_jira_with_fields(
            "ACCS-1",
            None,
            &ContentFormat::Jfm,
            FieldSelection::Named(vec!["Acceptance Criteria".to_string()]),
            &client,
            &instance_url,
        )
        .await;
        assert!(result.is_ok());
    }

    // ── run_write_jira_with_resolved_fields ────────────────────────

    #[tokio::test]
    async fn run_write_jira_with_resolved_fields_force_applies_payload() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "description": {"version": 1, "type": "doc", "content": []},
                    "summary": "New",
                    "customfield_10001": {"value": "Unplanned"}
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = AdfDocument::new();
        let mut custom = std::collections::BTreeMap::new();
        custom.insert(
            "customfield_10001".to_string(),
            serde_json::json!({"value": "Unplanned"}),
        );

        let result =
            run_write_jira_with_resolved_fields("ACCS-1", &adf, "New", true, &custom, &client)
                .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_write_jira_with_resolved_fields_empty_title_sends_no_summary() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/ACCS-1"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "fields": {
                    "description": {"version": 1, "type": "doc", "content": []},
                    "customfield_10001": 42
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let adf = AdfDocument::new();
        let mut custom = std::collections::BTreeMap::new();
        custom.insert("customfield_10001".to_string(), serde_json::json!(42));

        run_write_jira_with_resolved_fields("ACCS-1", &adf, "", true, &custom, &client)
            .await
            .unwrap();
    }

    #[test]
    fn print_jira_dry_run_with_scalars_and_sections() {
        use crate::atlassian::document::CustomFieldSection;
        let adf = AdfDocument::new();
        let mut scalars = std::collections::BTreeMap::new();
        scalars.insert(
            "Planned / Unplanned Work".to_string(),
            serde_yaml::Value::String("Unplanned".to_string()),
        );
        let sections = [CustomFieldSection {
            name: "Acceptance Criteria".to_string(),
            id: "customfield_19300".to_string(),
            body: "- item".to_string(),
        }];
        let result =
            print_jira_dry_run_with_custom_fields("ACCS-1", &adf, "T", &scalars, &sections);
        assert!(result.is_ok());
    }

    #[test]
    fn print_jira_dry_run_without_extras_still_prints_description() {
        let adf = AdfDocument::new();
        let scalars = std::collections::BTreeMap::new();
        let result = print_jira_dry_run_with_custom_fields("ACCS-1", &adf, "", &scalars, &[]);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_read_jira_with_fields_propagates_client_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        let instance_url = client.instance_url().to_string();

        let err = run_read_jira_with_fields(
            "NOPE-1",
            None,
            &ContentFormat::Jfm,
            FieldSelection::All,
            &client,
            &instance_url,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("404"));
    }
}
