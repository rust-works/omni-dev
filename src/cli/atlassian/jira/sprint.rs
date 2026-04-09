//! CLI commands for JIRA agile sprints.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AgileSprintList, JiraSearchResult};
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA agile sprints.
#[derive(Parser)]
pub struct SprintCommand {
    /// The sprint subcommand to execute.
    #[command(subcommand)]
    pub command: SprintSubcommands,
}

/// Sprint subcommands.
#[derive(Subcommand)]
pub enum SprintSubcommands {
    /// Lists sprints for a board.
    List(ListCommand),
    /// Lists issues in a sprint.
    Issues(IssuesCommand),
    /// Adds issues to a sprint.
    Add(AddCommand),
}

impl SprintCommand {
    /// Executes the sprint command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            SprintSubcommands::List(cmd) => cmd.execute().await,
            SprintSubcommands::Issues(cmd) => cmd.execute().await,
            SprintSubcommands::Add(cmd) => cmd.execute().await,
        }
    }
}

/// Lists sprints for a board.
#[derive(Parser)]
pub struct ListCommand {
    /// Board ID.
    #[arg(long)]
    pub board_id: u64,

    /// Filter by sprint state (active, future, closed).
    #[arg(long)]
    pub state: Option<String>,

    /// Maximum number of results (default: 50).
    #[arg(long, default_value_t = 50)]
    pub max_results: u32,
}

impl ListCommand {
    /// Fetches and displays sprints.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let result = client
            .get_sprints(self.board_id, self.state.as_deref(), self.max_results)
            .await?;
        print_sprints(&result);
        Ok(())
    }
}

/// Lists issues in a sprint.
#[derive(Parser)]
pub struct IssuesCommand {
    /// Sprint ID.
    #[arg(long)]
    pub sprint_id: u64,

    /// JQL filter for issues.
    #[arg(long)]
    pub jql: Option<String>,

    /// Maximum number of results (default: 50).
    #[arg(long, default_value_t = 50)]
    pub max_results: u32,
}

impl IssuesCommand {
    /// Fetches and displays sprint issues.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let result = client
            .get_sprint_issues(self.sprint_id, self.jql.as_deref(), self.max_results)
            .await?;
        print_sprint_issues(&result);
        Ok(())
    }
}

/// Adds issues to a sprint.
#[derive(Parser)]
pub struct AddCommand {
    /// Sprint ID.
    #[arg(long)]
    pub sprint_id: u64,

    /// Comma-separated issue keys (e.g., "PROJ-1,PROJ-2").
    #[arg(long)]
    pub issues: String,
}

impl AddCommand {
    /// Parses issue keys and adds them to the sprint.
    pub async fn execute(self) -> Result<()> {
        let keys = parse_issue_keys(&self.issues);
        if keys.is_empty() {
            anyhow::bail!("No issue keys provided. Use --issues KEY1,KEY2,...");
        }

        let (client, _instance_url) = create_client()?;
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        client
            .add_issues_to_sprint(self.sprint_id, &key_refs)
            .await?;

        println!(
            "Added {} issue(s) to sprint {}.",
            keys.len(),
            self.sprint_id
        );
        Ok(())
    }
}

/// Parses a comma-separated list of issue keys.
fn parse_issue_keys(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Prints sprints as a formatted table.
fn print_sprints(result: &AgileSprintList) {
    if result.sprints.is_empty() {
        println!("No sprints found.");
        return;
    }

    let id_width = result
        .sprints
        .iter()
        .map(|s| s.id.to_string().len())
        .max()
        .unwrap_or(2)
        .max(2);
    let state_width = result
        .sprints
        .iter()
        .map(|s| s.state.len())
        .max()
        .unwrap_or(5)
        .max(5);

    println!(
        "{:<id_width$}  {:<state_width$}  START       END         NAME",
        "ID", "STATE"
    );
    let name_sep = "-".repeat(4);
    println!(
        "{:<id_width$}  {:<state_width$}  ----------  ----------  {name_sep}",
        "-".repeat(id_width),
        "-".repeat(state_width),
    );

    for sprint in &result.sprints {
        let start = format_date(sprint.start_date.as_deref());
        let end = format_date(sprint.end_date.as_deref());
        println!(
            "{:<id_width$}  {:<state_width$}  {:<10}  {:<10}  {}",
            sprint.id, sprint.state, start, end, sprint.name
        );
    }

    if result.total > result.sprints.len() as u32 {
        println!(
            "\nShowing {} of {} sprints.",
            result.sprints.len(),
            result.total
        );
    }
}

/// Formats an ISO date string to just the date portion, or "-" if absent.
fn format_date(date: Option<&str>) -> &str {
    match date {
        Some(d) if d.len() >= 10 => &d[..10],
        Some(d) => d,
        None => "-",
    }
}

/// Prints sprint issues as a formatted table.
fn print_sprint_issues(result: &JiraSearchResult) {
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
    use crate::atlassian::client::{AgileSprint, JiraIssue};

    fn sample_sprint(
        id: u64,
        name: &str,
        state: &str,
        start: Option<&str>,
        end: Option<&str>,
        goal: Option<&str>,
    ) -> AgileSprint {
        AgileSprint {
            id,
            name: name.to_string(),
            state: state.to_string(),
            start_date: start.map(String::from),
            end_date: end.map(String::from),
            goal: goal.map(String::from),
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

    // ── parse_issue_keys ───────────────────────────────────────────

    #[test]
    fn parse_keys_basic() {
        let keys = parse_issue_keys("PROJ-1,PROJ-2,PROJ-3");
        assert_eq!(keys, vec!["PROJ-1", "PROJ-2", "PROJ-3"]);
    }

    #[test]
    fn parse_keys_with_whitespace() {
        let keys = parse_issue_keys("PROJ-1, PROJ-2 , PROJ-3");
        assert_eq!(keys, vec!["PROJ-1", "PROJ-2", "PROJ-3"]);
    }

    #[test]
    fn parse_keys_single() {
        let keys = parse_issue_keys("PROJ-1");
        assert_eq!(keys, vec!["PROJ-1"]);
    }

    #[test]
    fn parse_keys_empty() {
        let keys = parse_issue_keys("");
        assert!(keys.is_empty());
    }

    #[test]
    fn parse_keys_trailing_comma() {
        let keys = parse_issue_keys("PROJ-1,PROJ-2,");
        assert_eq!(keys, vec!["PROJ-1", "PROJ-2"]);
    }

    // ── format_date ────────────────────────────────────────────────

    #[test]
    fn format_date_full_iso() {
        assert_eq!(format_date(Some("2026-03-15T10:00:00.000Z")), "2026-03-15");
    }

    #[test]
    fn format_date_just_date() {
        assert_eq!(format_date(Some("2026-03-15")), "2026-03-15");
    }

    #[test]
    fn format_date_short() {
        assert_eq!(format_date(Some("2026")), "2026");
    }

    #[test]
    fn format_date_none() {
        assert_eq!(format_date(None), "-");
    }

    // ── print_sprints ──────────────────────────────────────────────

    #[test]
    fn print_sprints_empty() {
        let result = AgileSprintList {
            sprints: vec![],
            total: 0,
        };
        print_sprints(&result);
    }

    #[test]
    fn print_sprints_with_data() {
        let result = AgileSprintList {
            sprints: vec![
                sample_sprint(
                    10,
                    "Sprint 1",
                    "closed",
                    Some("2026-03-01"),
                    Some("2026-03-14"),
                    Some("MVP"),
                ),
                sample_sprint(11, "Sprint 2", "active", Some("2026-03-15"), None, None),
            ],
            total: 2,
        };
        print_sprints(&result);
    }

    #[test]
    fn print_sprints_with_pagination() {
        let result = AgileSprintList {
            sprints: vec![sample_sprint(10, "Sprint 1", "active", None, None, None)],
            total: 50,
        };
        print_sprints(&result);
    }

    // ── print_sprint_issues ────────────────────────────────────────

    #[test]
    fn print_sprint_issues_empty() {
        let result = JiraSearchResult {
            issues: vec![],
            total: 0,
        };
        print_sprint_issues(&result);
    }

    #[test]
    fn print_sprint_issues_with_data() {
        let result = JiraSearchResult {
            issues: vec![
                sample_issue("PROJ-1", "Fix login", Some("Open"), Some("Alice")),
                sample_issue("PROJ-2", "Add feature", None, None),
            ],
            total: 2,
        };
        print_sprint_issues(&result);
    }

    #[test]
    fn print_sprint_issues_with_pagination() {
        let result = JiraSearchResult {
            issues: vec![sample_issue("PROJ-1", "Issue", Some("Open"), None)],
            total: 100,
        };
        print_sprint_issues(&result);
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn sprint_command_list_variant() {
        let cmd = SprintCommand {
            command: SprintSubcommands::List(ListCommand {
                board_id: 1,
                state: None,
                max_results: 50,
            }),
        };
        assert!(matches!(cmd.command, SprintSubcommands::List(_)));
    }

    #[test]
    fn sprint_command_issues_variant() {
        let cmd = SprintCommand {
            command: SprintSubcommands::Issues(IssuesCommand {
                sprint_id: 10,
                jql: None,
                max_results: 50,
            }),
        };
        assert!(matches!(cmd.command, SprintSubcommands::Issues(_)));
    }

    #[test]
    fn sprint_command_add_variant() {
        let cmd = SprintCommand {
            command: SprintSubcommands::Add(AddCommand {
                sprint_id: 10,
                issues: "PROJ-1,PROJ-2".to_string(),
            }),
        };
        assert!(matches!(cmd.command, SprintSubcommands::Add(_)));
    }

    #[test]
    fn list_command_with_state() {
        let cmd = ListCommand {
            board_id: 1,
            state: Some("active".to_string()),
            max_results: 25,
        };
        assert_eq!(cmd.state.as_deref(), Some("active"));
    }
}
