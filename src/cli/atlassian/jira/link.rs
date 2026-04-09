//! CLI commands for JIRA issue links.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::JiraLinkType;
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
            LinkSubcommands::Types(cmd) => cmd.execute().await,
            LinkSubcommands::Create(cmd) => cmd.execute().await,
            LinkSubcommands::Remove(cmd) => cmd.execute().await,
            LinkSubcommands::Epic(cmd) => cmd.execute().await,
        }
    }
}

/// Lists available issue link types.
#[derive(Parser)]
pub struct TypesCommand;

impl TypesCommand {
    /// Fetches and displays link types.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let types = client.get_link_types().await?;
        print_link_types(&types);
        Ok(())
    }
}

/// Creates a link between two issues.
#[derive(Parser)]
pub struct CreateLinkCommand {
    /// Link type name (e.g., "Blocks", "Clones").
    #[arg(long, value_name = "TYPE")]
    pub r#type: String,

    /// Inward issue key (e.g., the issue that "is blocked by").
    #[arg(long)]
    pub inward: String,

    /// Outward issue key (e.g., the issue that "blocks").
    #[arg(long)]
    pub outward: String,
}

impl CreateLinkCommand {
    /// Creates the issue link.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        client
            .create_issue_link(&self.r#type, &self.inward, &self.outward)
            .await?;
        println!(
            "Linked {} {} {} (type: {}).",
            self.inward,
            format_link_direction(&self.r#type),
            self.outward,
            self.r#type
        );
        Ok(())
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
        client.remove_issue_link(&self.link_id).await?;
        println!("Removed link {}.", self.link_id);
        Ok(())
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
        client.link_to_epic(&self.epic, &self.issue).await?;
        println!("Linked {} to epic {}.", self.issue, self.epic);
        Ok(())
    }
}

/// Formats a link direction arrow for display.
fn format_link_direction(type_name: &str) -> &str {
    match type_name.to_lowercase().as_str() {
        "relates to" | "relates" => "↔",
        _ => "→",
    }
}

/// Prints link types as a formatted table.
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

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn link_command_types_variant() {
        let cmd = LinkCommand {
            command: LinkSubcommands::Types(TypesCommand),
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
}
