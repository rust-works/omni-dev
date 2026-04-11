//! CLI commands for JIRA agile boards.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AgileBoardList, JiraSearchResult};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA agile boards.
#[derive(Parser)]
pub struct BoardCommand {
    /// The board subcommand to execute.
    #[command(subcommand)]
    pub command: BoardSubcommands,
}

/// Board subcommands.
#[derive(Subcommand)]
pub enum BoardSubcommands {
    /// Lists agile boards.
    List(ListCommand),
    /// Lists issues on a board.
    Issues(IssuesCommand),
}

impl BoardCommand {
    /// Executes the board command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            BoardSubcommands::List(cmd) => cmd.execute().await,
            BoardSubcommands::Issues(cmd) => cmd.execute().await,
        }
    }
}

/// Lists agile boards.
#[derive(Parser)]
pub struct ListCommand {
    /// Filter by project key.
    #[arg(long)]
    pub project: Option<String>,

    /// Filter by board type (scrum or kanban).
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Maximum number of results, 0 for unlimited (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays boards.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let result = client
            .get_boards(self.project.as_deref(), self.r#type.as_deref(), self.limit)
            .await?;
        if output_as(&result, &self.output)? {
            return Ok(());
        }
        print_boards(&result);
        Ok(())
    }
}

/// Lists issues on a board.
#[derive(Parser)]
pub struct IssuesCommand {
    /// Board ID.
    #[arg(long)]
    pub board_id: u64,

    /// JQL filter for issues.
    #[arg(long)]
    pub jql: Option<String>,

    /// Maximum number of results, 0 for unlimited (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl IssuesCommand {
    /// Fetches and displays board issues.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let result = client
            .get_board_issues(self.board_id, self.jql.as_deref(), self.limit)
            .await?;
        if output_as(&result, &self.output)? {
            return Ok(());
        }
        print_board_issues(&result);
        Ok(())
    }
}

/// Prints boards as a formatted table.
fn print_boards(result: &AgileBoardList) {
    if result.boards.is_empty() {
        println!("No boards found.");
        return;
    }

    let id_width = result
        .boards
        .iter()
        .map(|b| b.id.to_string().len())
        .max()
        .unwrap_or(2)
        .max(2);
    let type_width = result
        .boards
        .iter()
        .map(|b| b.board_type.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let proj_width = result
        .boards
        .iter()
        .filter_map(|b| b.project_key.as_ref().map(String::len))
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<id_width$}  {:<type_width$}  {:<proj_width$}  NAME",
        "ID", "TYPE", "PROJECT"
    );
    let name_sep = "-".repeat(4);
    println!(
        "{:<id_width$}  {:<type_width$}  {:<proj_width$}  {name_sep}",
        "-".repeat(id_width),
        "-".repeat(type_width),
        "-".repeat(proj_width),
    );

    for board in &result.boards {
        let proj = board.project_key.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<type_width$}  {:<proj_width$}  {}",
            board.id, board.board_type, proj, board.name
        );
    }

    if result.total > result.boards.len() as u32 {
        println!(
            "\nShowing {} of {} boards.",
            result.boards.len(),
            result.total
        );
    }
}

/// Prints board issues as a formatted table (reuses search result format).
fn print_board_issues(result: &JiraSearchResult) {
    if result.issues.is_empty() {
        println!("No issues found.");
        return;
    }

    let key_width = result
        .issues
        .iter()
        .map(|i| i.key.len())
        .max()
        .unwrap_or(3)
        .max(3);
    let status_width = result
        .issues
        .iter()
        .filter_map(|i| i.status.as_ref().map(String::len))
        .max()
        .unwrap_or(6)
        .max(6);
    let assignee_width = result
        .issues
        .iter()
        .filter_map(|i| i.assignee.as_ref().map(String::len))
        .max()
        .unwrap_or(8)
        .max(8);

    let summary_sep = "-".repeat(7);
    println!(
        "{:<key_width$}  {:<status_width$}  {:<assignee_width$}  SUMMARY",
        "KEY", "STATUS", "ASSIGNEE"
    );
    println!(
        "{:<key_width$}  {:<status_width$}  {:<assignee_width$}  {summary_sep}",
        "-".repeat(key_width),
        "-".repeat(status_width),
        "-".repeat(assignee_width),
    );

    for issue in &result.issues {
        let status = issue.status.as_deref().unwrap_or("-");
        let assignee = issue.assignee.as_deref().unwrap_or("-");
        println!(
            "{:<key_width$}  {:<status_width$}  {:<assignee_width$}  {}",
            issue.key, status, assignee, issue.summary
        );
    }

    if result.total > result.issues.len() as u32 {
        println!(
            "\nShowing {} of {} issues.",
            result.issues.len(),
            result.total
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::{AgileBoard, JiraIssue};

    fn sample_board(id: u64, name: &str, board_type: &str, project: Option<&str>) -> AgileBoard {
        AgileBoard {
            id,
            name: name.to_string(),
            board_type: board_type.to_string(),
            project_key: project.map(String::from),
        }
    }

    fn sample_issue(
        key: &str,
        summary: &str,
        status: Option<&str>,
        assignee: Option<&str>,
    ) -> JiraIssue {
        JiraIssue {
            key: key.to_string(),
            summary: summary.to_string(),
            description_adf: None,
            status: status.map(String::from),
            issue_type: None,
            assignee: assignee.map(String::from),
            priority: None,
            labels: vec![],
        }
    }

    // ── print_boards ───────────────────────────────────────────────

    #[test]
    fn print_boards_empty() {
        let result = AgileBoardList {
            boards: vec![],
            total: 0,
        };
        print_boards(&result);
    }

    #[test]
    fn print_boards_with_data() {
        let result = AgileBoardList {
            boards: vec![
                sample_board(1, "PROJ Board", "scrum", Some("PROJ")),
                sample_board(2, "Kanban", "kanban", None),
            ],
            total: 2,
        };
        print_boards(&result);
    }

    #[test]
    fn print_boards_with_pagination() {
        let result = AgileBoardList {
            boards: vec![sample_board(1, "Board", "scrum", Some("PROJ"))],
            total: 100,
        };
        print_boards(&result);
    }

    // ── print_board_issues ─────────────────────────────────────────

    #[test]
    fn print_board_issues_empty() {
        let result = JiraSearchResult {
            issues: vec![],
            total: 0,
        };
        print_board_issues(&result);
    }

    #[test]
    fn print_board_issues_with_data() {
        let result = JiraSearchResult {
            issues: vec![
                sample_issue("PROJ-1", "Fix login", Some("Open"), Some("Alice")),
                sample_issue("PROJ-2", "Add feature", None, None),
            ],
            total: 2,
        };
        print_board_issues(&result);
    }

    #[test]
    fn print_board_issues_with_pagination() {
        let result = JiraSearchResult {
            issues: vec![sample_issue("PROJ-1", "Issue", Some("Open"), None)],
            total: 50,
        };
        print_board_issues(&result);
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn board_command_list_variant() {
        let cmd = BoardCommand {
            command: BoardSubcommands::List(ListCommand {
                project: None,
                r#type: None,
                limit: 50,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, BoardSubcommands::List(_)));
    }

    #[test]
    fn board_command_issues_variant() {
        let cmd = BoardCommand {
            command: BoardSubcommands::Issues(IssuesCommand {
                board_id: 1,
                jql: None,
                limit: 50,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, BoardSubcommands::Issues(_)));
    }

    #[test]
    fn list_command_with_filters() {
        let cmd = ListCommand {
            project: Some("PROJ".to_string()),
            r#type: Some("scrum".to_string()),
            limit: 25,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.project.as_deref(), Some("PROJ"));
        assert_eq!(cmd.r#type.as_deref(), Some("scrum"));
    }

    #[test]
    fn issues_command_with_jql() {
        let cmd = IssuesCommand {
            board_id: 42,
            jql: Some("status = Open".to_string()),
            limit: 10,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.board_id, 42);
        assert_eq!(cmd.jql.as_deref(), Some("status = Open"));
    }
}
