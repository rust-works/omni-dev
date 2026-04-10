//! CLI commands for listing JIRA projects.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::JiraProjectList;
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA projects.
#[derive(Parser)]
pub struct ProjectCommand {
    /// The project subcommand to execute.
    #[command(subcommand)]
    pub command: ProjectSubcommands,
}

/// Project subcommands.
#[derive(Subcommand)]
pub enum ProjectSubcommands {
    /// Lists all accessible JIRA projects.
    List(ListCommand),
}

impl ProjectCommand {
    /// Executes the project command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            ProjectSubcommands::List(cmd) => cmd.execute().await,
        }
    }
}

/// Lists all accessible JIRA projects.
#[derive(Parser)]
pub struct ListCommand {
    /// Maximum number of results, 0 for unlimited (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
}

impl ListCommand {
    /// Fetches and displays projects.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let result = client.get_projects(self.limit).await?;
        print_projects(&result);
        Ok(())
    }
}

/// Prints projects as a formatted table.
fn print_projects(result: &JiraProjectList) {
    if result.projects.is_empty() {
        println!("No projects found.");
        return;
    }

    let key_width = result
        .projects
        .iter()
        .map(|p| p.key.len())
        .max()
        .unwrap_or(3)
        .max(3);
    let type_width = result
        .projects
        .iter()
        .filter_map(|p| p.project_type.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);
    let lead_width = result
        .projects
        .iter()
        .filter_map(|p| p.lead.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);

    println!(
        "{:<key_width$}  {:<type_width$}  {:<lead_width$}  NAME",
        "KEY", "TYPE", "LEAD"
    );
    let name_sep = "-".repeat(4);
    println!(
        "{:<key_width$}  {:<type_width$}  {:<lead_width$}  {name_sep}",
        "-".repeat(key_width),
        "-".repeat(type_width),
        "-".repeat(lead_width),
    );

    for project in &result.projects {
        let ptype = project.project_type.as_deref().unwrap_or("-");
        let lead = project.lead.as_deref().unwrap_or("-");
        println!(
            "{:<key_width$}  {:<type_width$}  {:<lead_width$}  {}",
            project.key, ptype, lead, project.name
        );
    }

    if result.total > result.projects.len() as u32 {
        println!(
            "\nShowing {} of {} projects.",
            result.projects.len(),
            result.total
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::JiraProject;

    fn sample_project(
        key: &str,
        name: &str,
        ptype: Option<&str>,
        lead: Option<&str>,
    ) -> JiraProject {
        JiraProject {
            id: "1".to_string(),
            key: key.to_string(),
            name: name.to_string(),
            project_type: ptype.map(String::from),
            lead: lead.map(String::from),
        }
    }

    // ── print_projects ─────────────────────────────────────────────

    #[test]
    fn print_projects_empty() {
        let result = JiraProjectList {
            projects: vec![],
            total: 0,
        };
        print_projects(&result);
    }

    #[test]
    fn print_projects_with_data() {
        let result = JiraProjectList {
            projects: vec![
                sample_project("PROJ", "My Project", Some("software"), Some("Alice")),
                sample_project("OPS", "Operations", Some("business"), None),
            ],
            total: 2,
        };
        print_projects(&result);
    }

    #[test]
    fn print_projects_with_pagination() {
        let result = JiraProjectList {
            projects: vec![sample_project(
                "PROJ",
                "My Project",
                Some("software"),
                Some("Alice"),
            )],
            total: 100,
        };
        print_projects(&result);
    }

    #[test]
    fn print_projects_all_fields_none() {
        let result = JiraProjectList {
            projects: vec![sample_project("X", "Minimal", None, None)],
            total: 1,
        };
        print_projects(&result);
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn project_command_list_variant() {
        let cmd = ProjectCommand {
            command: ProjectSubcommands::List(ListCommand { limit: 50 }),
        };
        assert!(matches!(cmd.command, ProjectSubcommands::List(_)));
    }

    #[test]
    fn list_command_defaults() {
        let cmd = ListCommand { limit: 50 };
        assert_eq!(cmd.limit, 50);
    }
}
