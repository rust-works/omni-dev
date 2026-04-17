//! CLI commands for JIRA issue links.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AtlassianClient, JiraIssueLink, JiraLinkType};
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
    /// Lists links on a JIRA issue.
    List(ListLinksCommand),
    /// Lists available issue link types.
    Types(TypesCommand),
    /// Creates a link between two issues.
    Create(CreateLinkCommand),
    /// Removes an issue link by ID.
    Remove(RemoveLinkCommand),
    /// Links an issue to an epic (sets parent).
    Epic(EpicLinkCommand),
}

impl LinkCommand {
    /// Executes the link command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            LinkSubcommands::List(cmd) => cmd.execute().await,
            LinkSubcommands::Types(cmd) => cmd.execute().await,
            LinkSubcommands::Create(cmd) => cmd.execute().await,
            LinkSubcommands::Remove(cmd) => cmd.execute().await,
            LinkSubcommands::Epic(cmd) => cmd.execute().await,
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
}

impl RemoveLinkCommand {
    /// Removes the issue link.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_remove_link(&client, &self.link_id).await
    }
}

/// Links an issue to an epic.
#[derive(Parser)]
pub struct EpicLinkCommand {
    /// Epic issue key.
    #[arg(long)]
    pub epic: String,

    /// Issue key to link to the epic.
    #[arg(long)]
    pub issue: String,
}

impl EpicLinkCommand {
    /// Sets the epic as the parent of the issue.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_epic_link(&client, &self.epic, &self.issue).await
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

/// Removes an issue link by ID.
async fn run_remove_link(client: &AtlassianClient, link_id: &str) -> Result<()> {
    client.remove_issue_link(link_id).await?;
    println!("Removed link {link_id}.");
    Ok(())
}

/// Links an issue to an epic.
async fn run_epic_link(client: &AtlassianClient, epic: &str, issue: &str) -> Result<()> {
    client.link_to_epic(epic, issue).await?;
    println!("Linked {issue} to epic {epic}.");
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
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::Remove(_)));
    }

    #[test]
    fn link_command_epic_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::Epic(EpicLinkCommand {
                epic: "EPIC-1".to_string(),
                issue: "PROJ-2".to_string(),
            }),
        };
        assert!(matches!(cmd.command, LinkSubcommands::Epic(_)));
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
    fn epic_link_command_fields() {
        let cmd = EpicLinkCommand {
            epic: "EPIC-1".to_string(),
            issue: "STORY-1".to_string(),
        };
        assert_eq!(cmd.epic, "EPIC-1");
        assert_eq!(cmd.issue, "STORY-1");
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
    async fn run_remove_link_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/12345"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_remove_link(&client, "12345").await.is_ok());
    }

    #[tokio::test]
    async fn run_remove_link_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/issueLink/99999"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_remove_link(&client, "99999").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_epic_link_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-2"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_epic_link(&client, "EPIC-1", "PROJ-2").await.is_ok());
    }

    #[tokio::test]
    async fn run_epic_link_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-2"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_epic_link(&client, "EPIC-1", "PROJ-2")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }
}
