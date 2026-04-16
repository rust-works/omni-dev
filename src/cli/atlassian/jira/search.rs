//! CLI command for searching JIRA issues with JQL.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::{AtlassianClient, JiraSearchResult};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Searches JIRA issues using JQL.
#[derive(Parser)]
pub struct SearchCommand {
    /// Raw JQL query string (e.g., "project = PROJ AND status = Open").
    #[arg(long)]
    pub jql: Option<String>,

    /// Filter by project key.
    #[arg(long)]
    pub project: Option<String>,

    /// Filter by assignee (display name or email).
    #[arg(long)]
    pub assignee: Option<String>,

    /// Filter by status name.
    #[arg(long)]
    pub status: Option<String>,

    /// Maximum number of results, 0 for unlimited (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SearchCommand {
    /// Executes the search and prints results as a table.
    pub async fn execute(self) -> Result<()> {
        let jql = self.build_jql()?;
        let (client, _instance_url) = create_client()?;
        run_search(&client, &jql, self.limit, &self.output).await
    }

    /// Builds a JQL query from the provided flags, or returns the raw `--jql` value.
    fn build_jql(&self) -> Result<String> {
        if let Some(ref jql) = self.jql {
            return Ok(jql.clone());
        }

        let mut clauses = Vec::new();

        if let Some(ref project) = self.project {
            clauses.push(format!("project = \"{project}\""));
        }
        if let Some(ref assignee) = self.assignee {
            clauses.push(format!("assignee = \"{assignee}\""));
        }
        if let Some(ref status) = self.status {
            clauses.push(format!("status = \"{status}\""));
        }

        if clauses.is_empty() {
            anyhow::bail!(
                "Provide --jql for a raw query, or at least one filter flag (--project, --assignee, --status)"
            );
        }

        Ok(clauses.join(" AND "))
    }
}

/// Searches issues by JQL and displays results.
async fn run_search(
    client: &AtlassianClient,
    jql: &str,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let result = client.search_issues(jql, limit).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    print_search_results(&result);
    Ok(())
}

/// Prints search results: empty message, table, or table with pagination note.
fn print_search_results(result: &JiraSearchResult) {
    if result.issues.is_empty() {
        println!("No issues found.");
        return;
    }

    // Calculate column widths
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

    // Header
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

    // Rows
    for issue in &result.issues {
        let status = issue.status.as_deref().unwrap_or("-");
        let assignee = issue.assignee.as_deref().unwrap_or("-");
        println!(
            "{:<key_width$}  {:<status_width$}  {:<assignee_width$}  {}",
            issue.key, status, assignee, issue.summary
        );
    }

    // Pagination note
    if result.total > result.issues.len() as u32 {
        println!(
            "\nShowing {} of {} results.",
            result.issues.len(),
            result.total
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::JiraIssue;

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

    // ── build_jql ──────────────────────────────────────────────────

    #[test]
    fn build_jql_from_raw() {
        let cmd = SearchCommand {
            jql: Some("project = PROJ ORDER BY created".to_string()),
            project: None,
            assignee: None,
            status: None,
            limit: 50,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.build_jql().unwrap(), "project = PROJ ORDER BY created");
    }

    #[test]
    fn build_jql_from_project() {
        let cmd = SearchCommand {
            jql: None,
            project: Some("PROJ".to_string()),
            assignee: None,
            status: None,
            limit: 50,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.build_jql().unwrap(), "project = \"PROJ\"");
    }

    #[test]
    fn build_jql_from_multiple_flags() {
        let cmd = SearchCommand {
            jql: None,
            project: Some("PROJ".to_string()),
            assignee: Some("alice".to_string()),
            status: Some("Open".to_string()),
            limit: 25,
            output: OutputFormat::Table,
        };
        let jql = cmd.build_jql().unwrap();
        assert!(jql.contains("project = \"PROJ\""));
        assert!(jql.contains("assignee = \"alice\""));
        assert!(jql.contains("status = \"Open\""));
        assert!(jql.contains(" AND "));
    }

    #[test]
    fn build_jql_no_flags_errors() {
        let cmd = SearchCommand {
            jql: None,
            project: None,
            assignee: None,
            status: None,
            limit: 50,
            output: OutputFormat::Table,
        };
        assert!(cmd.build_jql().is_err());
    }

    #[test]
    fn build_jql_raw_overrides_flags() {
        let cmd = SearchCommand {
            jql: Some("assignee = bob".to_string()),
            project: Some("PROJ".to_string()),
            assignee: Some("alice".to_string()),
            status: None,
            limit: 50,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.build_jql().unwrap(), "assignee = bob");
    }

    // ── print_search_results ───────────────────────────────────────

    #[test]
    fn print_results_empty() {
        let result = JiraSearchResult {
            issues: vec![],
            total: 0,
        };
        // Should print "No issues found." and not panic
        print_search_results(&result);
    }

    #[test]
    fn print_results_with_issues() {
        let result = JiraSearchResult {
            issues: vec![
                sample_issue("PROJ-1", "Fix login", Some("Open"), Some("Alice")),
                sample_issue("PROJ-2", "Add feature", None, None),
            ],
            total: 2,
        };
        // Should print table without pagination note
        print_search_results(&result);
    }

    #[test]
    fn print_results_with_pagination() {
        let result = JiraSearchResult {
            issues: vec![sample_issue(
                "PROJ-1",
                "First issue",
                Some("Open"),
                Some("Alice"),
            )],
            total: 100,
        };
        // Should print table plus "Showing 1 of 100 results."
        print_search_results(&result);
    }

    #[test]
    fn print_results_all_fields_none() {
        let result = JiraSearchResult {
            issues: vec![sample_issue("X-1", "Minimal", None, None)],
            total: 1,
        };
        // Should use "-" for missing status/assignee
        print_search_results(&result);
    }
}
