//! CLI commands for JIRA project components.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::AtlassianClient;
use crate::atlassian::jira_types::JiraComponent;
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA project components.
#[derive(Parser)]
pub struct ComponentCommand {
    /// The component subcommand to execute.
    #[command(subcommand)]
    pub command: ComponentSubcommands,
}

/// Component subcommands.
#[derive(Subcommand)]
pub enum ComponentSubcommands {
    /// Lists components for a project (mirrors the `jira_component_list` MCP tool).
    List(ListCommand),
    /// Creates a new project component (mirrors the `jira_component_create` MCP tool).
    Create(CreateCommand),
    /// Updates a component's name/description (mirrors the `jira_component_update` MCP tool).
    Update(UpdateCommand),
    /// Deletes a component (mirrors the `jira_component_delete` MCP tool).
    Delete(DeleteCommand),
}

impl ComponentCommand {
    /// Executes the component command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            ComponentSubcommands::List(cmd) => cmd.execute().await,
            ComponentSubcommands::Create(cmd) => cmd.execute().await,
            ComponentSubcommands::Update(cmd) => cmd.execute().await,
            ComponentSubcommands::Delete(cmd) => cmd.execute().await,
        }
    }
}

/// Lists components for a JIRA project.
#[derive(Parser)]
pub struct ListCommand {
    /// Project key (e.g., "PROJ").
    #[arg(long)]
    pub project: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays components.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let components = client.get_project_components(&self.project).await?;
        if output_as(&components, &self.output)? {
            return Ok(());
        }
        print_components(&components);
        Ok(())
    }
}

/// Creates a component on a JIRA project.
#[derive(Parser)]
pub struct CreateCommand {
    /// Project key (e.g., "PROJ").
    #[arg(long)]
    pub project: String,

    /// Component name.
    #[arg(long)]
    pub name: String,

    /// Component description.
    #[arg(long)]
    pub description: Option<String>,
}

impl CreateCommand {
    /// Creates the component.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let component = client
            .create_component(&self.project, &self.name, self.description.as_deref())
            .await?;
        println!(
            "Created component {} (id: {}).",
            component.name, component.id
        );
        Ok(())
    }
}

/// Updates a JIRA component.
#[derive(Parser)]
pub struct UpdateCommand {
    /// Component ID (from `component list`).
    pub component_id: String,

    /// New component name.
    #[arg(long)]
    pub name: Option<String>,

    /// New component description.
    #[arg(long)]
    pub description: Option<String>,
}

impl UpdateCommand {
    /// Updates the component.
    pub async fn execute(self) -> Result<()> {
        if self.name.is_none() && self.description.is_none() {
            anyhow::bail!("Nothing to update: pass --name and/or --description.");
        }
        let (client, _instance_url) = create_client()?;
        client
            .update_component(
                &self.component_id,
                self.name.as_deref(),
                self.description.as_deref(),
            )
            .await?;
        println!("Updated component {}.", self.component_id);
        Ok(())
    }
}

/// Deletes a JIRA component.
#[derive(Parser)]
pub struct DeleteCommand {
    /// Component ID (from `component list`).
    pub component_id: String,

    /// Reassign issues referencing this component to this component id before
    /// deleting (otherwise the references are dropped).
    #[arg(long)]
    pub move_issues_to: Option<String>,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be deleted without making any API calls.
    #[arg(long)]
    pub dry_run: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let mut reader = std::io::BufReader::new(std::io::stdin());
        let mut writer = std::io::stdout();
        self.execute_with_io(&client, &mut reader, &mut writer)
            .await
    }

    /// Inner form taking explicit client and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        client: &AtlassianClient,
        reader: &mut (dyn std::io::BufRead + Send),
        writer: &mut (dyn std::io::Write + Send),
    ) -> Result<()> {
        if !self.force || self.dry_run {
            let prompt = format!("Delete component {}? [y/N] ", self.component_id);
            let dry_run_message = format!("Would delete component {}.", self.component_id);
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
                GuardOutcome::Cancelled | GuardOutcome::DryRun => return Ok(()),
                GuardOutcome::Proceed => {}
            }
        }

        client
            .delete_component(&self.component_id, self.move_issues_to.as_deref())
            .await?;
        writeln!(writer, "Deleted component {}.", self.component_id)?;
        Ok(())
    }
}

/// Prints components as a simple table.
fn print_components(components: &[JiraComponent]) {
    if components.is_empty() {
        println!("No components.");
        return;
    }
    println!("{:<12} {:<30} DESCRIPTION", "ID", "NAME");
    for c in components {
        println!(
            "{:<12} {:<30} {}",
            c.id,
            c.name,
            c.description.as_deref().unwrap_or("")
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    #[test]
    fn component_command_list_variant() {
        let cmd = ComponentCommand {
            command: ComponentSubcommands::List(ListCommand {
                project: "PROJ".to_string(),
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, ComponentSubcommands::List(_)));
    }

    #[test]
    fn component_command_delete_variant() {
        let cmd = ComponentCommand {
            command: ComponentSubcommands::Delete(DeleteCommand {
                component_id: "10000".to_string(),
                move_issues_to: None,
                force: false,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, ComponentSubcommands::Delete(_)));
    }

    #[test]
    fn print_components_empty_and_populated() {
        print_components(&[]);
        print_components(&[JiraComponent {
            id: "1".to_string(),
            name: "Backend".to_string(),
            description: Some("Server".to_string()),
        }]);
    }

    #[tokio::test]
    async fn update_with_no_fields_errors_before_client() {
        let cmd = UpdateCommand {
            component_id: "10000".to_string(),
            name: None,
            description: None,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("Nothing to update"));
    }

    #[tokio::test]
    async fn delete_component_force_calls_delete() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/component/10000"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            component_id: "10000".to_string(),
            move_issues_to: None,
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Deleted component 10000."));
    }

    #[tokio::test]
    async fn delete_component_dry_run_makes_no_api_call() {
        let client = mock_client("http://127.0.0.1:1");
        let cmd = DeleteCommand {
            component_id: "10000".to_string(),
            move_issues_to: None,
            force: false,
            dry_run: true,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Would delete component 10000."));
        assert!(!out.contains("Deleted component"));
    }
}
