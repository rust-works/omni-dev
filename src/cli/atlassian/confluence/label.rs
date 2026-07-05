//! CLI commands for managing Confluence page labels.

use anyhow::Result;
use clap::{Parser, Subcommand};

use std::io::{self, BufRead, Write};

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::atlassian::confluence_types::ConfluenceLabel;
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
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
    /// Lists labels on a Confluence page (mirrors the `confluence_label_list` MCP tool).
    List(ListCommand),
    /// Adds labels to a Confluence page (mirrors the `confluence_label_add` MCP tool).
    Add(AddCommand),
    /// Removes labels from a Confluence page (mirrors the `confluence_label_remove` MCP tool).
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
        run_list(&api, &self.id, &self.output).await
    }
}

/// Fetches and displays labels for a page.
async fn run_list(api: &ConfluenceApi, id: &str, output: &OutputFormat) -> Result<()> {
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
        run_add(&api, &self.id, &self.labels).await
    }
}

/// Adds labels to a page and prints confirmation.
async fn run_add(api: &ConfluenceApi, id: &str, labels: &[String]) -> Result<()> {
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

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be removed without making any API calls.
    #[arg(long)]
    pub dry_run: bool,
}

impl RemoveCommand {
    /// Executes the remove labels command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let mut reader = io::BufReader::new(io::stdin());
        let mut writer = io::stdout();
        self.execute_with_io(&api, &mut reader, &mut writer).await
    }

    /// Inner form taking explicit API and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        api: &ConfluenceApi,
        reader: &mut (dyn BufRead + Send),
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        let joined = self.labels.join(", ");
        let count = self.labels.len();
        let prompt = format!(
            "Remove {count} label(s) [{joined}] from page {}? [y/N] ",
            self.id
        );
        let dry_run_message = format!(
            "Would remove {count} label(s) from page {}: {joined}.",
            self.id
        );

        let outcome = guard_destructive_with_io(
            &GuardOptions {
                prompt: &prompt,
                dry_run_message: &dry_run_message,
                force: self.force,
                dry_run: self.dry_run,
            },
            reader,
            writer,
        )?;

        match outcome {
            GuardOutcome::Proceed => {
                for label in &self.labels {
                    api.remove_label(&self.id, label).await?;
                }
                writeln!(writer, "Removed {} label(s) from page {}.", count, self.id)?;
                Ok(())
            }
            GuardOutcome::Cancelled | GuardOutcome::DryRun => Ok(()),
        }
    }
}

/// Prints labels as a formatted table.
fn print_labels(labels: &[crate::atlassian::confluence_types::ConfluenceLabel]) {
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
                force: true,
                dry_run: false,
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

    // ── run_list (wiremock) ──────────────────────────────────

    #[tokio::test]
    async fn run_list_table_output() {
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
        assert!(run_list(&api, "12345", &OutputFormat::Table).await.is_ok());
    }

    #[tokio::test]
    async fn run_list_json_output() {
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
        assert!(run_list(&api, "12345", &OutputFormat::Json).await.is_ok());
    }

    // ── run_add (wiremock) ────────────────────────────────────

    #[tokio::test]
    async fn run_add_success() {
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
        assert!(run_add(&api, "12345", &["arch".to_string()]).await.is_ok());
    }

    // ── run_remove (wiremock) ─────────────────────────────────

    fn label_api(server: &wiremock::MockServer) -> ConfluenceApi {
        let client =
            crate::atlassian::client::AtlassianClient::new(&server.uri(), "u@t.com", "tok")
                .unwrap();
        ConfluenceApi::new(client)
    }

    #[tokio::test]
    async fn remove_execute_force_calls_api() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/draft",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string()],
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&api, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Removed 1 label(s) from page 12345."));
    }

    #[tokio::test]
    async fn remove_execute_force_calls_api_for_each_label() {
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

        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string(), "old".to_string()],
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&api, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Removed 2 label(s) from page 12345."));
    }

    #[tokio::test]
    async fn remove_execute_dry_run_skips_api_and_lists_labels() {
        let server = wiremock::MockServer::start().await;
        // No mocks: any API call would fail.

        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string(), "old".to_string()],
            force: false,
            dry_run: true,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&api, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Would remove 2 label(s) from page 12345: draft, old."));
        assert!(!out.contains("Removed"));
    }

    #[tokio::test]
    async fn remove_execute_prompt_yes_calls_api() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/draft",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string()],
            force: false,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&api, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Remove 1 label(s) [draft] from page 12345?"));
        assert!(out.contains("Removed 1 label(s) from page 12345."));
    }

    #[tokio::test]
    async fn remove_execute_prompt_no_skips_api() {
        let server = wiremock::MockServer::start().await;
        // No mocks: any API call would fail.

        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string()],
            force: false,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&api, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Cancelled."));
        assert!(!out.contains("Removed"));
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
            force: false,
            dry_run: false,
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.labels, vec!["test"]);
        assert!(!cmd.force);
        assert!(!cmd.dry_run);
    }

    #[test]
    fn remove_command_dry_run_field() {
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["test".to_string()],
            force: false,
            dry_run: true,
        };
        assert!(cmd.dry_run);
    }

    #[tokio::test]
    async fn remove_execute_force_propagates_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/draft",
            ))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string()],
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = cmd
            .execute_with_io(&api, &mut input, &mut output)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    /// Force-mode + failing writer covers `?` on the post-API writeln.
    #[tokio::test]
    async fn remove_execute_force_propagates_writeln_error() {
        use crate::test_support::failing_io::FailingWriter;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/draft",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string()],
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(&api, &mut input, &mut writer)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// Dry-run with a failing writer covers `?` on guard_destructive_with_io.
    #[tokio::test]
    async fn remove_execute_dry_run_propagates_guard_error() {
        use crate::test_support::failing_io::FailingWriter;
        let server = wiremock::MockServer::start().await;
        let api = label_api(&server);
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string()],
            force: false,
            dry_run: true,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(&api, &mut input, &mut writer)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// End-to-end exercise of the public `RemoveCommand::execute()`
    /// wrapper.
    #[tokio::test]
    async fn remove_execute_drives_create_client_and_calls_api() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/label/draft",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        let cmd = RemoveCommand {
            id: "12345".to_string(),
            labels: vec!["draft".to_string()],
            force: true,
            dry_run: false,
        };
        cmd.execute().await.unwrap();
    }
}
