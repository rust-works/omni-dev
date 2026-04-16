//! CLI commands for JIRA issue worklogs.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::JiraWorklogList;
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages worklogs (time tracking) on a JIRA issue.
#[derive(Parser)]
pub struct WorklogCommand {
    /// The worklog subcommand to execute.
    #[command(subcommand)]
    pub command: WorklogSubcommands,
}

/// Worklog subcommands.
#[derive(Subcommand)]
pub enum WorklogSubcommands {
    /// Lists worklogs on a JIRA issue.
    List(ListCommand),
    /// Adds a worklog entry to a JIRA issue.
    Add(AddCommand),
}

impl WorklogCommand {
    /// Executes the worklog command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WorklogSubcommands::List(cmd) => cmd.execute().await,
            WorklogSubcommands::Add(cmd) => cmd.execute().await,
        }
    }
}

/// Lists worklogs on a JIRA issue.
#[derive(Parser)]
pub struct ListCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Maximum number of results, 0 for unlimited (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays worklogs.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let result = client.get_worklogs(&self.key, self.limit).await?;
        if output_as(&result, &self.output)? {
            return Ok(());
        }
        print_worklogs(&result);
        Ok(())
    }
}

/// Adds a worklog entry to a JIRA issue.
#[derive(Parser)]
pub struct AddCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Time spent in JIRA duration format (e.g., "2h 30m", "1d", "45m").
    #[arg(long)]
    pub time_spent: String,

    /// When the work was started (ISO 8601, e.g., "2026-04-16T09:00:00.000+0000").
    #[arg(long)]
    pub started: Option<String>,

    /// Comment describing the work performed.
    #[arg(long)]
    pub comment: Option<String>,
}

impl AddCommand {
    /// Posts the worklog entry.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        client
            .add_worklog(
                &self.key,
                &self.time_spent,
                self.started.as_deref(),
                self.comment.as_deref(),
            )
            .await?;

        println!("Worklog added to {} ({}).", self.key, self.time_spent);
        Ok(())
    }
}

/// Prints worklogs as a formatted table.
fn print_worklogs(result: &JiraWorklogList) {
    if result.worklogs.is_empty() {
        println!("No worklogs.");
        return;
    }

    let author_width = result
        .worklogs
        .iter()
        .map(|w| w.author.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let time_width = result
        .worklogs
        .iter()
        .map(|w| w.time_spent.len())
        .max()
        .unwrap_or(5)
        .max(5);

    println!(
        "{:<author_width$}  {:<time_width$}  STARTED     COMMENT",
        "AUTHOR", "SPENT"
    );
    let comment_sep = "-".repeat(7);
    println!(
        "{:<author_width$}  {:<time_width$}  ----------  {comment_sep}",
        "-".repeat(author_width),
        "-".repeat(time_width),
    );

    for worklog in &result.worklogs {
        let started = format_date(&worklog.started);
        let comment = worklog.comment.as_deref().unwrap_or("-");
        // Truncate long comments for table display.
        let comment_display = if comment.len() > 60 {
            format!("{}...", &comment[..57])
        } else {
            comment.to_string()
        };
        println!(
            "{:<author_width$}  {:<time_width$}  {:<10}  {}",
            worklog.author, worklog.time_spent, started, comment_display
        );
    }

    if result.total > result.worklogs.len() as u32 {
        println!(
            "\nShowing {} of {} worklogs.",
            result.worklogs.len(),
            result.total
        );
    }
}

/// Formats an ISO date string to just the date portion.
fn format_date(date: &str) -> &str {
    if date.len() >= 10 {
        &date[..10]
    } else {
        date
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::JiraWorklog;

    fn sample_worklog(
        id: &str,
        author: &str,
        time_spent: &str,
        seconds: u64,
        started: &str,
        comment: Option<&str>,
    ) -> JiraWorklog {
        JiraWorklog {
            id: id.to_string(),
            author: author.to_string(),
            time_spent: time_spent.to_string(),
            time_spent_seconds: seconds,
            started: started.to_string(),
            comment: comment.map(String::from),
        }
    }

    // ── format_date ────────────────────────────────────────────────

    #[test]
    fn format_date_full_iso() {
        assert_eq!(format_date("2026-04-16T09:00:00.000+0000"), "2026-04-16");
    }

    #[test]
    fn format_date_just_date() {
        assert_eq!(format_date("2026-04-16"), "2026-04-16");
    }

    #[test]
    fn format_date_short() {
        assert_eq!(format_date("2026"), "2026");
    }

    #[test]
    fn format_date_empty() {
        assert_eq!(format_date(""), "");
    }

    // ── print_worklogs ─────────────────────────────────────────────

    #[test]
    fn print_worklogs_empty() {
        let result = JiraWorklogList {
            worklogs: vec![],
            total: 0,
        };
        print_worklogs(&result);
    }

    #[test]
    fn print_worklogs_with_data() {
        let result = JiraWorklogList {
            worklogs: vec![
                sample_worklog(
                    "1",
                    "Alice",
                    "2h",
                    7200,
                    "2026-04-16T09:00:00.000+0000",
                    Some("Debugging login"),
                ),
                sample_worklog(
                    "2",
                    "Bob",
                    "1d",
                    28800,
                    "2026-04-15T10:00:00.000+0000",
                    None,
                ),
            ],
            total: 2,
        };
        print_worklogs(&result);
    }

    #[test]
    fn print_worklogs_with_pagination() {
        let result = JiraWorklogList {
            worklogs: vec![sample_worklog(
                "1",
                "Alice",
                "30m",
                1800,
                "2026-04-16",
                None,
            )],
            total: 50,
        };
        print_worklogs(&result);
    }

    #[test]
    fn print_worklogs_long_comment_truncated() {
        let long_comment = "a".repeat(100);
        let result = JiraWorklogList {
            worklogs: vec![sample_worklog(
                "1",
                "Alice",
                "1h",
                3600,
                "2026-04-16",
                Some(&long_comment),
            )],
            total: 1,
        };
        print_worklogs(&result);
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn worklog_command_list_variant() {
        let cmd = WorklogCommand {
            command: WorklogSubcommands::List(ListCommand {
                key: "PROJ-1".to_string(),
                limit: 50,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, WorklogSubcommands::List(_)));
    }

    #[test]
    fn worklog_command_add_variant() {
        let cmd = WorklogCommand {
            command: WorklogSubcommands::Add(AddCommand {
                key: "PROJ-1".to_string(),
                time_spent: "2h".to_string(),
                started: None,
                comment: None,
            }),
        };
        assert!(matches!(cmd.command, WorklogSubcommands::Add(_)));
    }

    #[test]
    fn add_command_all_fields() {
        let cmd = AddCommand {
            key: "PROJ-1".to_string(),
            time_spent: "2h 30m".to_string(),
            started: Some("2026-04-16T09:00:00.000+0000".to_string()),
            comment: Some("Fixed the bug".to_string()),
        };
        assert_eq!(cmd.key, "PROJ-1");
        assert_eq!(cmd.time_spent, "2h 30m");
        assert_eq!(cmd.started.as_deref(), Some("2026-04-16T09:00:00.000+0000"));
        assert_eq!(cmd.comment.as_deref(), Some("Fixed the bug"));
    }

    #[test]
    fn list_command_custom_limit() {
        let cmd = ListCommand {
            key: "PROJ-1".to_string(),
            limit: 10,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.limit, 10);
    }
}
