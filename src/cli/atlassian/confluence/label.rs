//! CLI commands for managing Confluence page labels.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::confluence_api::{ConfluenceApi, ConfluenceLabel};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages labels on Confluence pages.
#[derive(Parser)]
pub struct LabelCommand {
    /// The label subcommand to execute.
    #[command(subcommand)]
    pub command: LabelSubcommands,
}

/// Label subcommands.
#[derive(Subcommand)]
pub enum LabelSubcommands {
    /// Lists labels on a Confluence page.
    List(ListCommand),
    /// Adds labels to a Confluence page.
    Add(AddCommand),
    /// Removes labels from a Confluence page.
    Remove(RemoveCommand),
}

impl LabelCommand {
    /// Executes the label command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            LabelSubcommands::List(cmd) => cmd.execute().await,
            LabelSubcommands::Add(cmd) => cmd.execute().await,
            LabelSubcommands::Remove(cmd) => cmd.execute().await,
        }
    }
}

/// Lists labels on a Confluence page.
#[derive(Parser)]
pub struct ListCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Executes the list labels command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        execute_list(&api, &self.id, &self.output).await
    }
}

/// Fetches and displays labels for a page.
async fn execute_list(api: &ConfluenceApi, id: &str, output: &OutputFormat) -> Result<()> {
    let labels = api.get_labels(id).await?;
    display_labels(&labels, output)
}

/// Formats and displays labels in the requested output format.
fn display_labels(labels: &Vec<ConfluenceLabel>, output: &OutputFormat) -> Result<()> {
    if output_as(labels, output)? {
        return Ok(());
    }
    print_labels(labels);
    Ok(())
}

/// Adds labels to a Confluence page.
#[derive(Parser)]
pub struct AddCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Comma-separated list of labels to add.
    #[arg(long, value_delimiter = ',', required = true)]
    pub labels: Vec<String>,
}

impl AddCommand {
    /// Executes the add labels command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        execute_add(&api, &self.id, &self.labels).await
    }
}

/// Adds labels to a page and prints confirmation.
async fn execute_add(api: &ConfluenceApi, id: &str, labels: &[String]) -> Result<()> {
    api.add_labels(id, labels).await?;
    print_add_confirmation(labels.len(), id);
    Ok(())
}

/// Prints confirmation after adding labels.
fn print_add_confirmation(count: usize, id: &str) {
    println!("Added {count} label(s) to page {id}.");
}

/// Removes labels from a Confluence page.
#[derive(Parser)]
pub struct RemoveCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Comma-separated list of labels to remove.
    #[arg(long, value_delimiter = ',', required = true)]
    pub labels: Vec<String>,
}

impl RemoveCommand {
    /// Executes the remove labels command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        execute_remove(&api, &self.id, &self.labels).await
    }
}

/// Removes labels from a page and prints confirmation.
async fn execute_remove(api: &ConfluenceApi, id: &str, labels: &[String]) -> Result<()> {
    for label in labels {
        api.remove_label(id, label).await?;
    }
    print_remove_confirmation(labels.len(), id);
    Ok(())
}

/// Prints confirmation after removing labels.
fn print_remove_confirmation(count: usize, id: &str) {
    println!("Removed {count} label(s) from page {id}.");
}

/// Prints labels as a formatted table.
fn print_labels(labels: &[crate::atlassian::confluence_api::ConfluenceLabel]) {
    if labels.is_empty() {
        println!("No labels found.");
        return;
    }

    let name_width = labels
        .iter()
        .map(|l| l.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let prefix_width = labels
        .iter()
        .map(|l| l.prefix.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!("{:<name_width$}  {:<prefix_width$}", "NAME", "PREFIX");
    println!(
        "{:<name_width$}  {:<prefix_width$}",
        "-".repeat(name_width),
        "-".repeat(prefix_width),
    );

    for label in labels {
        println!(
            "{:<name_width$}  {:<prefix_width$}",
            label.name, label.prefix
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_label(id: &str, name: &str, prefix: &str) -> ConfluenceLabel {
        ConfluenceLabel {
            id: id.to_string(),
            name: name.to_string(),
            prefix: prefix.to_string(),
        }
    }

    // ── LabelCommand struct ───────────────────────────────────────

    #[test]
    fn label_subcommands_list_variant() {
        let cmd = LabelCommand {
            command: LabelSubcommands::List(ListCommand {
                id: "12345".to_string(),
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, LabelSubcommands::List(_)));
    }

    #[test]
    fn label_subcommands_add_variant() {
        let cmd = LabelCommand {
            command: LabelSubcommands::Add(AddCommand {
                id: "12345".to_string(),
                labels: vec!["architecture".to_string(), "draft".to_string()],
            }),
        };
        assert!(matches!(cmd.command, LabelSubcommands::Add(_)));
    }

    #[test]
    fn label_subcommands_remove_variant() {
        let cmd = LabelCommand {
            command: LabelSubcommands::Remove(RemoveCommand {
                id: "12345".to_string(),
                labels: vec!["draft".to_string()],
            }),
        };
        assert!(matches!(cmd.command, LabelSubcommands::Remove(_)));
    }

    // ── display_labels ────────────────────────────────────────────

    #[test]
    fn display_labels_table() {
        let labels = vec![
            sample_label("1", "architecture", "global"),
            sample_label("2", "draft", "global"),
        ];
        assert!(display_labels(&labels, &OutputFormat::Table).is_ok());
    }

    #[test]
    fn display_labels_json() {
        let labels = vec![sample_label("1", "architecture", "global")];
        assert!(display_labels(&labels, &OutputFormat::Json).is_ok());
    }

    #[test]
    fn display_labels_yaml() {
        let labels = vec![sample_label("1", "architecture", "global")];
        assert!(display_labels(&labels, &OutputFormat::Yaml).is_ok());
    }

    #[test]
    fn display_labels_empty_table() {
        assert!(display_labels(&vec![], &OutputFormat::Table).is_ok());
    }

    // ── print_labels ──────────────────────────────────────────────

    #[test]
    fn print_labels_empty() {
        print_labels(&[]);
    }

    #[test]
    fn print_labels_with_entries() {
        let labels = vec![
            sample_label("1", "architecture", "global"),
            sample_label("2", "draft", "global"),
        ];
        print_labels(&labels);
    }

    // ── print confirmations ───────────────────────────────────────

    #[test]
    fn print_add_confirmation_single() {
        print_add_confirmation(1, "12345");
    }

    #[test]
    fn print_add_confirmation_multiple() {
        print_add_confirmation(3, "12345");
    }

    #[test]
    fn print_remove_confirmation_single() {
        print_remove_confirmation(1, "12345");
    }

    #[test]
    fn print_remove_confirmation_multiple() {
        print_remove_confirmation(2, "12345");
    }

    // ── execute_list (wiremock) ──────────────────────────────────

    #[tokio::test]
    async fn execute_list_table_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "name": "architecture", "prefix": "global"},
                        {"id": "2", "name": "draft", "prefix": "global"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(execute_list(&api, "12345", &OutputFormat::Table)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn execute_list_json_output() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345/labels"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"id": "1", "name": "architecture", "prefix": "global"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(execute_list(&api, "12345", &OutputFormat::Json)
            .await
            .is_ok());
    }

    // ── execute_add (wiremock) ────────────────────────────────────

    #[tokio::test]
    async fn execute_add_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{"prefix": "global", "name": "arch", "id": "1"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(execute_add(&api, "12345", &["arch".to_string()])
            .await
            .is_ok());
    }

    // ── execute_remove (wiremock) ─────────────────────────────────

    #[tokio::test]
    async fn execute_remove_success() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/draft",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(execute_remove(&api, "12345", &["draft".to_string()])
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn execute_remove_multiple() {
        let server = wiremock::MockServer::start().await;

        for label in &["draft", "old"] {
            wiremock::Mock::given(wiremock::matchers::method("DELETE"))
                .and(wiremock::matchers::path(format!(
                    "/wiki/rest/api/content/12345/label/{label}"
                )))
                .respond_with(wiremock::ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;
        }

        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        let api = ConfluenceApi::new(client);
        assert!(
            execute_remove(&api, "12345", &["draft".to_string(), "old".to_string()])
                .await
                .is_ok()
        );
    }

    // ── AddCommand struct ─────────────────────────────────────────

    #[test]
    fn add_command_fields() {
        let cmd = AddCommand {
            id: "12345".to_string(),
            labels: vec!["test".to_string()],
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.labels, vec!["test"]);
    }

    // ── RemoveCommand struct ──────────────────────────────────────

    #[test]
    fn remove_command_fields() {
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["test".to_string()],
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.labels, vec!["test"]);
    }
}
