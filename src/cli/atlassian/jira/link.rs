//! CLI commands for JIRA issue links.

use anyhow::Result;
use clap::{Parser, Subcommand};

use std::io::{self, BufRead, Write};

use crate::atlassian::client::{AtlassianClient, JiraIssueLink, JiraLinkType, JiraRemoteIssueLink};
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA issue links.
#[derive(Parser)]
pub struct LinkCommand {
    /// The link subcommand to execute.
    #[command(subcommand)]
    pub command: LinkSubcommands,
}

/// Link subcommands.
#[derive(Subcommand)]
pub enum LinkSubcommands {
    /// Lists links on a JIRA issue (mirrors the `jira_link_list` MCP tool).
    List(ListLinksCommand),
    /// Lists available issue link types (mirrors the `jira_link_types` MCP tool).
    Types(TypesCommand),
    /// Creates a link between two issues (mirrors the `jira_link_create` MCP tool).
    Create(CreateLinkCommand),
    /// Removes an issue link by ID (mirrors the `jira_link_remove` MCP tool).
    Remove(RemoveLinkCommand),
    /// Sets an issue's parent — Epic → Story or Story → Sub-task (mirrors the `jira_link_parent` MCP tool).
    #[command(alias = "epic")]
    Parent(ParentLinkCommand),
    /// Manages remote (external URL) issue links.
    Remote(RemoteLinkCommand),
}

impl LinkCommand {
    /// Executes the link command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            LinkSubcommands::List(cmd) => cmd.execute().await,
            LinkSubcommands::Types(cmd) => cmd.execute().await,
            LinkSubcommands::Create(cmd) => cmd.execute().await,
            LinkSubcommands::Remove(cmd) => cmd.execute().await,
            LinkSubcommands::Parent(cmd) => cmd.execute().await,
            LinkSubcommands::Remote(cmd) => cmd.execute().await,
        }
    }
}

/// Lists links on a JIRA issue.
#[derive(Parser)]
pub struct ListLinksCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListLinksCommand {
    /// Fetches and displays issue links.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list_links(&client, &self.key, &self.output).await
    }
}

/// Lists available issue link types.
#[derive(Parser)]
pub struct TypesCommand {
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl TypesCommand {
    /// Fetches and displays link types.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_link_types(&client, &self.output).await
    }
}

/// Creates a link between two issues.
#[derive(Parser)]
pub struct CreateLinkCommand {
    /// Link type name (e.g., "Blocks", "Clones").
    #[arg(long, value_name = "TYPE")]
    pub r#type: String,

    /// Source issue key (e.g., for "Blocks": the issue doing the blocking).
    #[arg(long)]
    pub inward: String,

    /// Target issue key (e.g., for "Blocks": the issue being blocked).
    #[arg(long)]
    pub outward: String,
}

impl CreateLinkCommand {
    /// Creates the issue link.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_create_link(&client, &self.r#type, &self.inward, &self.outward).await
    }
}

/// Removes an issue link by ID.
#[derive(Parser)]
pub struct RemoveLinkCommand {
    /// Link ID to remove.
    #[arg(long)]
    pub link_id: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be removed without making any API calls.
    #[arg(long)]
    pub dry_run: bool,
}

impl RemoveLinkCommand {
    /// Removes the issue link.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let mut reader = io::BufReader::new(io::stdin());
        let mut writer = io::stdout();
        self.execute_with_io(&client, &mut reader, &mut writer)
            .await
    }

    /// Inner form taking explicit client and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        client: &AtlassianClient,
        reader: &mut (dyn BufRead + Send),
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        let prompt = format!("Remove link {}? [y/N] ", self.link_id);
        let dry_run_message = format!("Would remove link {}.", self.link_id);

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
                client.remove_issue_link(&self.link_id).await?;
                writeln!(writer, "Removed link {}.", self.link_id)?;
                Ok(())
            }
            GuardOutcome::Cancelled | GuardOutcome::DryRun => Ok(()),
        }
    }
}

/// Sets an issue's parent (e.g., Epic → Story or Story → Sub-task).
#[derive(Parser)]
pub struct ParentLinkCommand {
    /// Parent issue key (e.g., the epic). Accepts `--epic` as an alias.
    #[arg(long, alias = "epic")]
    pub parent: String,

    /// Child issue key to place under the parent. Accepts `--issue` as an alias.
    #[arg(long, alias = "issue")]
    pub child: String,
}

impl ParentLinkCommand {
    /// Sets the parent of the child issue.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_parent_link(&client, &self.parent, &self.child).await
    }
}

/// Manages JIRA remote (external URL) issue links.
#[derive(Parser)]
pub struct RemoteLinkCommand {
    /// The remote-link subcommand to execute.
    #[command(subcommand)]
    pub command: RemoteLinkSubcommands,
}

/// Remote link subcommands.
#[derive(Subcommand)]
pub enum RemoteLinkSubcommands {
    /// Lists remote (external URL) links on a JIRA issue (mirrors the `jira_link_remote_list` MCP tool).
    List(ListRemoteLinksCommand),
}

impl RemoteLinkCommand {
    /// Executes the remote-link command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            RemoteLinkSubcommands::List(cmd) => cmd.execute().await,
        }
    }
}

/// Lists remote (external URL) links on a JIRA issue.
#[derive(Parser)]
pub struct ListRemoteLinksCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListRemoteLinksCommand {
    /// Fetches and displays remote issue links.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list_remote_links(&client, &self.key, &self.output).await
    }
}

/// Fetches and displays issue links.
async fn run_list_links(client: &AtlassianClient, key: &str, output: &OutputFormat) -> Result<()> {
    let links = client.get_issue_links(key).await?;
    if output_as(&links, output)? {
        return Ok(());
    }
    print_issue_links(key, &links);
    Ok(())
}

/// Fetches and displays link types.
async fn run_link_types(client: &AtlassianClient, output: &OutputFormat) -> Result<()> {
    let types = client.get_link_types().await?;
    if output_as(&types, output)? {
        return Ok(());
    }
    print_link_types(&types);
    Ok(())
}

/// Creates a link between two issues.
async fn run_create_link(
    client: &AtlassianClient,
    link_type: &str,
    inward: &str,
    outward: &str,
) -> Result<()> {
    client.create_issue_link(link_type, inward, outward).await?;
    println!(
        "Linked {} {} {} (type: {}).",
        inward,
        format_link_direction(link_type),
        outward,
        link_type
    );
    Ok(())
}

/// Sets the parent of an issue.
async fn run_parent_link(client: &AtlassianClient, parent: &str, child: &str) -> Result<()> {
    client.set_issue_parent(child, parent).await?;
    println!("Set parent of {child} to {parent}.");
    Ok(())
}

/// Formats a link direction arrow for display.
fn format_link_direction(type_name: &str) -> &str {
    match type_name.to_lowercase().as_str() {
        "relates to" | "relates" => "↔",
        _ => "→",
    }
}

/// Prints issue links as a formatted table.
fn print_issue_links(key: &str, links: &[JiraIssueLink]) {
    if links.is_empty() {
        println!("{key}: no links.");
        return;
    }

    let id_width = links.iter().map(|l| l.id.len()).max().unwrap_or(2).max(2);
    let type_width = links
        .iter()
        .map(|l| l.link_type.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let dir_width = 7; // "outward" is the longest
    let key_width = links
        .iter()
        .map(|l| l.linked_issue_key.len())
        .max()
        .unwrap_or(3)
        .max(3);

    println!(
        "{:<id_width$}  {:<type_width$}  {:<dir_width$}  {:<key_width$}  SUMMARY",
        "ID", "TYPE", "DIR", "KEY"
    );
    let summary_sep = "-".repeat(7);
    println!(
        "{:<id_width$}  {:<type_width$}  {:<dir_width$}  {:<key_width$}  {summary_sep}",
        "-".repeat(id_width),
        "-".repeat(type_width),
        "-".repeat(dir_width),
        "-".repeat(key_width),
    );

    for link in links {
        println!(
            "{:<id_width$}  {:<type_width$}  {:<dir_width$}  {:<key_width$}  {}",
            link.id,
            link.link_type,
            link.direction,
            link.linked_issue_key,
            link.linked_issue_summary
        );
    }
}

fn print_link_types(types: &[JiraLinkType]) {
    if types.is_empty() {
        println!("No link types found.");
        return;
    }

    let id_width = types.iter().map(|t| t.id.len()).max().unwrap_or(2).max(2);
    let name_width = types.iter().map(|t| t.name.len()).max().unwrap_or(4).max(4);
    let inward_width = types
        .iter()
        .map(|t| t.inward.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!(
        "{:<id_width$}  {:<name_width$}  {:<inward_width$}  OUTWARD",
        "ID", "NAME", "INWARD"
    );
    let out_sep = "-".repeat(7);
    println!(
        "{:<id_width$}  {:<name_width$}  {:<inward_width$}  {out_sep}",
        "-".repeat(id_width),
        "-".repeat(name_width),
        "-".repeat(inward_width),
    );

    for t in types {
        println!(
            "{:<id_width$}  {:<name_width$}  {:<inward_width$}  {}",
            t.id, t.name, t.inward, t.outward
        );
    }
}

/// Fetches and displays remote (external URL) issue links.
async fn run_list_remote_links(
    client: &AtlassianClient,
    key: &str,
    output: &OutputFormat,
) -> Result<()> {
    let links = client.get_remote_issue_links(key).await?;
    if output_as(&links, output)? {
        return Ok(());
    }
    print_remote_links(key, &links);
    Ok(())
}

/// Prints remote (external URL) issue links as a formatted table.
fn print_remote_links(key: &str, links: &[JiraRemoteIssueLink]) {
    if links.is_empty() {
        println!("{key}: no remote links.");
        return;
    }

    let id_width = links.iter().map(|l| l.id.len()).max().unwrap_or(2).max(2);
    let rel_width = links
        .iter()
        .map(|l| l.relationship.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(12)
        .max(12); // "RELATIONSHIP"
    let title_width = links
        .iter()
        .map(|l| l.object.title.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(5)
        .max(5); // "TITLE"

    println!(
        "{:<id_width$}  {:<rel_width$}  {:<title_width$}  URL",
        "ID", "RELATIONSHIP", "TITLE"
    );
    let url_sep = "-".repeat(3);
    println!(
        "{:<id_width$}  {:<rel_width$}  {:<title_width$}  {url_sep}",
        "-".repeat(id_width),
        "-".repeat(rel_width),
        "-".repeat(title_width),
    );

    for link in links {
        println!(
            "{:<id_width$}  {:<rel_width$}  {:<title_width$}  {}",
            link.id,
            link.relationship.as_deref().unwrap_or("-"),
            link.object.title.as_deref().unwrap_or("-"),
            link.object.url,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_link_type(id: &str, name: &str, inward: &str, outward: &str) -> JiraLinkType {
        JiraLinkType {
            id: id.to_string(),
            name: name.to_string(),
            inward: inward.to_string(),
            outward: outward.to_string(),
        }
    }

    // ── format_link_direction ──────────────────────────────────────

    #[test]
    fn format_direction_blocks() {
        assert_eq!(format_link_direction("Blocks"), "→");
    }

    #[test]
    fn format_direction_relates() {
        assert_eq!(format_link_direction("Relates to"), "↔");
    }

    #[test]
    fn format_direction_unknown() {
        assert_eq!(format_link_direction("Custom Link"), "→");
    }

    #[test]
    fn format_direction_case_insensitive() {
        assert_eq!(format_link_direction("BLOCKS"), "→");
        assert_eq!(format_link_direction("duplicates"), "→");
    }

    // ── print_link_types ───────────────────────────────────────────

    #[test]
    fn print_types_empty() {
        print_link_types(&[]);
    }

    #[test]
    fn print_types_with_data() {
        let types = vec![
            sample_link_type("1", "Blocks", "is blocked by", "blocks"),
            sample_link_type("2", "Clones", "is cloned by", "clones"),
        ];
        print_link_types(&types);
    }

    // ── print_issue_links ───────────────────────────────────────────

    fn sample_link(
        id: &str,
        link_type: &str,
        direction: &str,
        key: &str,
        summary: &str,
    ) -> JiraIssueLink {
        JiraIssueLink {
            id: id.to_string(),
            link_type: link_type.to_string(),
            direction: direction.to_string(),
            linked_issue_key: key.to_string(),
            linked_issue_summary: summary.to_string(),
        }
    }

    #[test]
    fn print_issue_links_empty() {
        print_issue_links("PROJ-1", &[]);
    }

    #[test]
    fn print_issue_links_with_data() {
        let links = vec![
            sample_link("100", "Blocks", "outward", "PROJ-2", "Blocked issue"),
            sample_link("101", "Relates", "inward", "PROJ-3", "Related issue"),
        ];
        print_issue_links("PROJ-1", &links);
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn link_command_list_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::List(ListLinksCommand {
                key: "PROJ-1".to_string(),
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::List(_)));
    }

    #[test]
    fn link_command_types_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::Types(TypesCommand {
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::Types(_)));
    }

    #[test]
    fn link_command_create_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::Create(CreateLinkCommand {
                r#type: "Blocks".to_string(),
                inward: "PROJ-1".to_string(),
                outward: "PROJ-2".to_string(),
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::Create(_)));
    }

    #[test]
    fn link_command_remove_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::Remove(RemoveLinkCommand {
                link_id: "12345".to_string(),
                force: true,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::Remove(_)));
    }

    #[test]
    fn remove_link_command_dry_run_field_default_false() {
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: false,
            dry_run: false,
        };
        assert!(!cmd.force);
        assert!(!cmd.dry_run);
    }

    #[test]
    fn link_command_parent_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::Parent(ParentLinkCommand {
                parent: "EPIC-1".to_string(),
                child: "PROJ-2".to_string(),
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::Parent(_)));
    }

    // ── struct fields ──────────────────────────────────────────────

    #[test]
    fn create_link_command_fields() {
        let cmd = CreateLinkCommand {
            r#type: "Blocks".to_string(),
            inward: "PROJ-1".to_string(),
            outward: "PROJ-2".to_string(),
        };
        assert_eq!(cmd.r#type, "Blocks");
        assert_eq!(cmd.inward, "PROJ-1");
        assert_eq!(cmd.outward, "PROJ-2");
    }

    #[test]
    fn parent_link_command_fields() {
        let cmd = ParentLinkCommand {
            parent: "EPIC-1".to_string(),
            child: "STORY-1".to_string(),
        };
        assert_eq!(cmd.parent, "EPIC-1");
        assert_eq!(cmd.child, "STORY-1");
    }

    // ── run_* link functions ───────────────────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    #[tokio::test]
    async fn run_list_links_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "Issue",
                        "issuelinks": []
                    }
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_list_links(&client, "PROJ-1", &OutputFormat::Table)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_list_links_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/NOPE-1"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_list_links(&client, "NOPE-1", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_link_types_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issueLinkType"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "issueLinkTypes": [
                        {"id": "1", "name": "Blocks", "inward": "is blocked by", "outward": "blocks"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_link_types(&client, &OutputFormat::Table).await.is_ok());
    }

    #[tokio::test]
    async fn run_link_types_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issueLinkType"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_link_types(&client, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn run_create_link_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_create_link(&client, "Blocks", "PROJ-1", "PROJ-2")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_create_link_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("Bad"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_create_link(&client, "Bad", "PROJ-1", "PROJ-2")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn remove_link_execute_force_calls_api() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Removed link 12345."));
    }

    #[tokio::test]
    async fn remove_link_execute_dry_run_skips_api() {
        let server = wiremock::MockServer::start().await;

        let client = mock_client(&server.uri());
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: false,
            dry_run: true,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Would remove link 12345."));
        assert!(!out.contains("Removed link"));
    }

    #[tokio::test]
    async fn remove_link_execute_prompt_yes_calls_api() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: false,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Remove link 12345?"));
        assert!(out.contains("Removed link 12345."));
    }

    #[tokio::test]
    async fn remove_link_execute_prompt_no_skips_api() {
        let server = wiremock::MockServer::start().await;

        let client = mock_client(&server.uri());
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: false,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Cancelled."));
        assert!(!out.contains("Removed link"));
    }

    #[tokio::test]
    async fn remove_link_execute_api_error_propagates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = RemoveLinkCommand {
            link_id: "99999".to_string(),
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = cmd
            .execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_parent_link_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-2"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_parent_link(&client, "EPIC-1", "PROJ-2").await.is_ok());
    }

    #[tokio::test]
    async fn run_parent_link_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-2"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_parent_link(&client, "EPIC-1", "PROJ-2")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    /// Force-mode + failing writer covers `?` on the post-API writeln.
    #[tokio::test]
    async fn remove_link_execute_force_propagates_writeln_error() {
        use crate::test_support::failing_io::FailingWriter;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(&client, &mut input, &mut writer)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// Dry-run with a failing writer covers `?` on guard_destructive_with_io.
    #[tokio::test]
    async fn remove_link_execute_dry_run_propagates_guard_error() {
        use crate::test_support::failing_io::FailingWriter;
        let server = wiremock::MockServer::start().await;
        let client = mock_client(&server.uri());
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: false,
            dry_run: true,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(&client, &mut input, &mut writer)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// End-to-end exercise of the public `RemoveLinkCommand::execute()`
    /// wrapper.
    #[tokio::test]
    async fn remove_link_execute_drives_create_client_and_calls_api() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        let cmd = RemoveLinkCommand {
            link_id: "12345".to_string(),
            force: true,
            dry_run: false,
        };
        cmd.execute().await.unwrap();
    }

    // ── remote links ───────────────────────────────────────────────

    use crate::atlassian::client::{JiraRemoteIssueLinkIcon, JiraRemoteIssueLinkObject};

    fn sample_remote_link(
        id: &str,
        relationship: Option<&str>,
        url: &str,
        title: Option<&str>,
        icon_title: Option<&str>,
    ) -> JiraRemoteIssueLink {
        JiraRemoteIssueLink {
            id: id.to_string(),
            global_id: None,
            relationship: relationship.map(String::from),
            object: JiraRemoteIssueLinkObject {
                url: url.to_string(),
                title: title.map(String::from),
                summary: None,
                icon: icon_title.map(|t| JiraRemoteIssueLinkIcon {
                    url: None,
                    title: Some(t.to_string()),
                }),
            },
        }
    }

    #[test]
    fn print_remote_links_empty() {
        // Just exercises the empty-print path; no panic.
        print_remote_links("PROJ-1", &[]);
    }

    #[test]
    fn print_remote_links_with_data() {
        let links = vec![
            sample_remote_link(
                "10001",
                Some("mentioned in"),
                "https://example.atlassian.net/wiki/page/1",
                Some("Design doc"),
                Some("Confluence Page"),
            ),
            sample_remote_link(
                "10002",
                None,
                "https://bitbucket.org/acme/repo/pull-requests/42",
                None,
                None,
            ),
        ];
        print_remote_links("PROJ-1", &links);
    }

    #[test]
    fn link_command_remote_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::Remote(RemoteLinkCommand {
                command: RemoteLinkSubcommands::List(ListRemoteLinksCommand {
                    key: "PROJ-1".to_string(),
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::Remote(_)));
    }

    #[tokio::test]
    async fn run_list_remote_links_table_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/remotelink",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "id": 10001,
                        "relationship": "mentioned in",
                        "object": {
                            "url": "https://example.atlassian.net/wiki/page/1",
                            "title": "Design doc"
                        }
                    }
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_list_remote_links(&client, "PROJ-1", &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_list_remote_links_yaml_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/remotelink",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_list_remote_links(&client, "PROJ-1", &OutputFormat::Yaml)
                .await
                .is_ok()
        );
    }

    /// End-to-end exercise of the nested public `execute()` chain for the
    /// remote-link `list` subcommand: outer `LinkCommand::execute` →
    /// `RemoteLinkCommand::execute` → `ListRemoteLinksCommand::execute` →
    /// `create_client` → API.
    #[tokio::test]
    async fn link_command_remote_list_execute_drives_create_client_and_calls_api() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/remotelink",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;

        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        let cmd = LinkCommand {
            command: LinkSubcommands::Remote(RemoteLinkCommand {
                command: RemoteLinkSubcommands::List(ListRemoteLinksCommand {
                    key: "PROJ-1".to_string(),
                    output: OutputFormat::Yaml,
                }),
            }),
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn run_list_remote_links_propagates_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/remotelink",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_list_remote_links(&client, "NOPE-1", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }
}
