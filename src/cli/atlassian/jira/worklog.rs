//! CLI commands for JIRA issue worklogs.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::AtlassianClient;
use crate::atlassian::jira_types::JiraWorklogList;
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
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
    /// Lists worklogs on a JIRA issue (mirrors the `jira_worklog_list` MCP tool).
    List(ListCommand),
    /// Adds a worklog entry to a JIRA issue (mirrors the `jira_worklog_add` MCP tool).
    Add(AddCommand),
    /// Edits a worklog entry on a JIRA issue (mirrors the `jira_worklog_update` MCP tool).
    Edit(EditCommand),
    /// Deletes a worklog entry from a JIRA issue (mirrors the `jira_worklog_delete` MCP tool).
    Delete(DeleteCommand),
}

impl WorklogCommand {
    /// Executes the worklog command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WorklogSubcommands::List(cmd) => cmd.execute().await,
            WorklogSubcommands::Add(cmd) => cmd.execute().await,
            WorklogSubcommands::Edit(cmd) => cmd.execute().await,
            WorklogSubcommands::Delete(cmd) => cmd.execute().await,
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

/// Edits an existing worklog entry on a JIRA issue.
#[derive(Parser)]
pub struct EditCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Worklog ID to edit (from `worklog list`).
    pub worklog_id: String,

    /// New time spent in JIRA duration format (e.g., "2h 30m").
    #[arg(long)]
    pub time_spent: Option<String>,

    /// New start time (ISO 8601, e.g., "2026-04-16T09:00:00.000+0000").
    #[arg(long)]
    pub started: Option<String>,

    /// New comment describing the work performed.
    #[arg(long)]
    pub comment: Option<String>,
}

impl EditCommand {
    /// Updates the worklog entry.
    pub async fn execute(self) -> Result<()> {
        if self.time_spent.is_none() && self.started.is_none() && self.comment.is_none() {
            anyhow::bail!(
                "Nothing to update: pass at least one of --time-spent, --started, or --comment."
            );
        }
        let (client, _instance_url) = create_client()?;
        client
            .update_worklog(
                &self.key,
                &self.worklog_id,
                self.time_spent.as_deref(),
                self.started.as_deref(),
                self.comment.as_deref(),
            )
            .await?;

        println!("Worklog {} updated on {}.", self.worklog_id, self.key);
        Ok(())
    }
}

/// Deletes a worklog entry from a JIRA issue.
#[derive(Parser)]
pub struct DeleteCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Worklog ID to delete (from `worklog list`).
    pub worklog_id: String,

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
            let prompt = format!("Delete worklog {} on {}? [y/N] ", self.worklog_id, self.key);
            let dry_run_message =
                format!("Would delete worklog {} on {}.", self.worklog_id, self.key);

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

        client.delete_worklog(&self.key, &self.worklog_id).await?;
        writeln!(
            writer,
            "Deleted worklog {} on {}.",
            self.worklog_id, self.key
        )?;

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
    use crate::atlassian::jira_types::JiraWorklog;

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

    #[test]
    fn worklog_command_edit_variant() {
        let cmd = WorklogCommand {
            command: WorklogSubcommands::Edit(EditCommand {
                key: "PROJ-1".to_string(),
                worklog_id: "100".to_string(),
                time_spent: Some("3h".to_string()),
                started: None,
                comment: None,
            }),
        };
        assert!(matches!(cmd.command, WorklogSubcommands::Edit(_)));
    }

    #[test]
    fn worklog_command_delete_variant() {
        let cmd = WorklogCommand {
            command: WorklogSubcommands::Delete(DeleteCommand {
                key: "PROJ-1".to_string(),
                worklog_id: "100".to_string(),
                force: false,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, WorklogSubcommands::Delete(_)));
    }

    // ── EditCommand ────────────────────────────────────────────────

    #[tokio::test]
    async fn edit_without_any_field_errors_before_client() {
        // All fields None → bail before create_client()/network.
        let cmd = EditCommand {
            key: "PROJ-1".to_string(),
            worklog_id: "100".to_string(),
            time_spent: None,
            started: None,
            comment: None,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("Nothing to update"));
    }

    // ── WorklogCommand::execute() end-to-end ───────────────────────
    //
    // Drives the top-level dispatch arm *and* each subcommand's execute()
    // wrapper (create_client + API call) against a wiremock.

    #[tokio::test]
    async fn worklog_command_execute_edit_drives_create_client() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/worklog/100",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "100"})),
            )
            .mount(&server)
            .await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        WorklogCommand {
            command: WorklogSubcommands::Edit(EditCommand {
                key: "PROJ-1".to_string(),
                worklog_id: "100".to_string(),
                time_spent: Some("3h".to_string()),
                started: None,
                comment: Some("Revised".to_string()),
            }),
        }
        .execute()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn worklog_command_execute_delete_drives_create_client() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/worklog/100",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        // `--force` skips the prompt, so the wrapper's real stdin is not read.
        WorklogCommand {
            command: WorklogSubcommands::Delete(DeleteCommand {
                key: "PROJ-1".to_string(),
                worklog_id: "100".to_string(),
                force: true,
                dry_run: false,
            }),
        }
        .execute()
        .await
        .unwrap();
    }

    // ── DeleteCommand ──────────────────────────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    #[tokio::test]
    async fn delete_worklog_force_calls_delete() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/worklog/100",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            worklog_id: "100".to_string(),
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
            .contains("Deleted worklog 100 on PROJ-1."));
    }

    #[tokio::test]
    async fn delete_worklog_dry_run_makes_no_api_call() {
        let client = mock_client("http://127.0.0.1:1");
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            worklog_id: "100".to_string(),
            force: false,
            dry_run: true,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Would delete worklog 100 on PROJ-1."));
        assert!(!out.contains("Deleted worklog"));
    }

    /// Answering "y" takes the `GuardOutcome::Proceed` arm.
    #[tokio::test]
    async fn delete_worklog_prompt_yes_calls_delete() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/worklog/100",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            worklog_id: "100".to_string(),
            force: false,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Delete worklog 100 on PROJ-1?"));
        assert!(out.contains("Deleted worklog 100 on PROJ-1."));
    }

    /// Dry-run with a failing writer covers the `?` on the guard call.
    #[tokio::test]
    async fn delete_worklog_dry_run_propagates_guard_error() {
        use crate::test_support::failing_io::FailingWriter;
        let client = mock_client("http://127.0.0.1:1");
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            worklog_id: "100".to_string(),
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

    /// Force + a failing writer covers the `?` on the post-success writeln.
    #[tokio::test]
    async fn delete_worklog_force_propagates_writeln_error() {
        use crate::test_support::failing_io::FailingWriter;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/worklog/100",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            worklog_id: "100".to_string(),
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

    #[tokio::test]
    async fn delete_worklog_prompt_no_makes_no_delete() {
        let client = mock_client("http://127.0.0.1:1");
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            worklog_id: "100".to_string(),
            force: false,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("Deleted worklog"));
    }
}
