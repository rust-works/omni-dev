//! CLI command for JIRA DevStatus (linked PRs, branches, repositories).

use anyhow::Result;
use clap::{Parser, ValueEnum};

use crate::atlassian::client::{
    JiraDevBranch, JiraDevCommit, JiraDevPullRequest, JiraDevRepository, JiraDevStatus,
    JiraDevStatusSummary,
};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Shows development status (PRs, branches, repositories) for a JIRA issue.
#[derive(Parser)]
pub struct DevCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Data type filter.
    #[arg(long)]
    pub r#type: Option<DevDataType>,

    /// Application type filter (e.g., GitHub, bitbucket, stash).
    /// If omitted, queries all available integrations.
    #[arg(long)]
    pub app: Option<String>,

    /// Show summary counts instead of full details.
    #[arg(long)]
    pub summary: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Development data type to retrieve.
#[derive(Clone, ValueEnum)]
pub enum DevDataType {
    /// Pull requests.
    Pullrequest,
    /// Branches.
    Branch,
    /// Repositories.
    Repository,
}

impl DevDataType {
    /// Returns the API data type string.
    fn as_api_str(&self) -> &str {
        match self {
            Self::Pullrequest => "pullrequest",
            Self::Branch => "branch",
            Self::Repository => "repository",
        }
    }
}

impl DevCommand {
    /// Executes the dev-status command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;

        if self.summary {
            let summary = client.get_dev_status_summary(&self.key).await?;
            if output_as(&summary, &self.output)? {
                return Ok(());
            }
            print_summary(&self.key, &summary);
            return Ok(());
        }

        let data_type = self.r#type.as_ref().map(DevDataType::as_api_str);
        let app_type = self.app.as_deref();
        let status = client
            .get_dev_status(&self.key, data_type, app_type)
            .await?;

        if output_as(&status, &self.output)? {
            return Ok(());
        }

        print_dev_status(&self.key, &status);
        Ok(())
    }
}

/// Prints a development status summary as a formatted table.
fn print_summary(key: &str, summary: &JiraDevStatusSummary) {
    if summary.pullrequest.count == 0 && summary.branch.count == 0 && summary.repository.count == 0
    {
        println!("{key}: no development information.");
        return;
    }

    println!("{key}:");

    let rows = [
        ("pullrequest", &summary.pullrequest),
        ("branch", &summary.branch),
        ("repository", &summary.repository),
    ];

    let type_w = rows.iter().map(|(t, _)| t.len()).max().unwrap_or(4).max(4);

    println!("  {:<type_w$}  {:>5}  PROVIDERS", "TYPE", "COUNT");
    println!(
        "  {:<type_w$}  {:>5}  {}",
        "-".repeat(type_w),
        "-----",
        "-".repeat(9),
    );

    for (name, count) in &rows {
        let providers = if count.providers.is_empty() {
            "-".to_string()
        } else {
            count.providers.join(", ")
        };
        println!("  {:<type_w$}  {:>5}  {}", name, count.count, providers);
    }
}

/// Prints development status as formatted tables.
fn print_dev_status(key: &str, status: &JiraDevStatus) {
    if status.pull_requests.is_empty()
        && status.branches.is_empty()
        && status.repositories.is_empty()
    {
        println!("{key}: no development information.");
        return;
    }

    if !status.pull_requests.is_empty() {
        print_pull_requests(&status.pull_requests);
    }

    if !status.branches.is_empty() {
        if !status.pull_requests.is_empty() {
            println!();
        }
        print_branches(&status.branches);
    }

    if !status.repositories.is_empty() {
        if !status.pull_requests.is_empty() || !status.branches.is_empty() {
            println!();
        }
        print_repositories(&status.repositories);
    }
}

/// Prints pull requests as a formatted table.
fn print_pull_requests(prs: &[JiraDevPullRequest]) {
    let id_w = prs.iter().map(|p| p.id.len()).max().unwrap_or(2).max(2);
    let status_w = prs.iter().map(|p| p.status.len()).max().unwrap_or(6).max(6);
    let author_w = prs
        .iter()
        .map(|p| p.author.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(6)
        .max(6);
    let repo_w = prs
        .iter()
        .map(|p| p.repository_name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let src_w = prs
        .iter()
        .map(|p| p.source_branch.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let dst_w = prs
        .iter()
        .map(|p| p.destination_branch.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!("Pull Requests:");
    println!(
        "  {:<id_w$}  {:<status_w$}  {:<author_w$}  {:<repo_w$}  {:<src_w$}  {:<dst_w$}  NAME",
        "ID", "STATUS", "AUTHOR", "REPO", "SOURCE", "TARGET"
    );
    println!(
        "  {:<id_w$}  {:<status_w$}  {:<author_w$}  {:<repo_w$}  {:<src_w$}  {:<dst_w$}  {}",
        "-".repeat(id_w),
        "-".repeat(status_w),
        "-".repeat(author_w),
        "-".repeat(repo_w),
        "-".repeat(src_w),
        "-".repeat(dst_w),
        "-".repeat(4),
    );

    for pr in prs {
        let author = pr.author.as_deref().unwrap_or("-");
        println!(
            "  {:<id_w$}  {:<status_w$}  {:<author_w$}  {:<repo_w$}  {:<src_w$}  {:<dst_w$}  {}",
            pr.id,
            pr.status,
            author,
            pr.repository_name,
            pr.source_branch,
            pr.destination_branch,
            pr.name
        );
    }
}

/// Prints branches as a formatted table.
fn print_branches(branches: &[JiraDevBranch]) {
    let repo_w = branches
        .iter()
        .map(|b| b.repository_name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let name_w = branches
        .iter()
        .map(|b| b.name.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let commit_w = 7; // short SHA length

    println!("Branches:");
    println!(
        "  {:<repo_w$}  {:<name_w$}  {:<commit_w$}  URL",
        "REPO", "BRANCH", "COMMIT"
    );
    println!(
        "  {:<repo_w$}  {:<name_w$}  {:<commit_w$}  {}",
        "-".repeat(repo_w),
        "-".repeat(name_w),
        "-".repeat(commit_w),
        "-".repeat(3),
    );

    for branch in branches {
        let commit = branch
            .last_commit
            .as_ref()
            .map_or("-", |c| c.display_id.as_str());
        println!(
            "  {:<repo_w$}  {:<name_w$}  {:<commit_w$}  {}",
            branch.repository_name, branch.name, commit, branch.url
        );
    }
}

/// Prints repositories as a formatted table, including commits.
fn print_repositories(repos: &[JiraDevRepository]) {
    let name_w = repos.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);

    println!("Repositories:");
    println!("  {:<name_w$}  URL", "NAME");
    println!("  {:<name_w$}  {}", "-".repeat(name_w), "-".repeat(3));

    for repo in repos {
        println!("  {:<name_w$}  {}", repo.name, repo.url);
        if !repo.commits.is_empty() {
            print_commits(&repo.commits);
        }
    }
}

/// Prints commits as a sub-table indented under a repository.
fn print_commits(commits: &[JiraDevCommit]) {
    let sha_w = 7; // short SHA
    let author_w = commits
        .iter()
        .map(|c| c.author.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!("    {:<sha_w$}  {:<author_w$}  MESSAGE", "SHA", "AUTHOR");

    for commit in commits {
        let author = commit.author.as_deref().unwrap_or("-");
        // Truncate message to first line.
        let msg = commit.message.lines().next().unwrap_or("");
        println!(
            "    {:<sha_w$}  {:<author_w$}  {}",
            commit.display_id, author, msg
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::JiraDevStatusCount;

    fn sample_pr() -> JiraDevPullRequest {
        JiraDevPullRequest {
            id: "#42".to_string(),
            name: "Fix login bug".to_string(),
            status: "MERGED".to_string(),
            url: "https://github.com/org/repo/pull/42".to_string(),
            repository_name: "org/repo".to_string(),
            source_branch: "fix-login".to_string(),
            destination_branch: "main".to_string(),
            author: Some("John Doe".to_string()),
            reviewers: vec!["Jane Smith".to_string()],
            comment_count: Some(3),
            last_update: Some("2024-01-15T10:30:00.000+0000".to_string()),
        }
    }

    fn sample_commit() -> JiraDevCommit {
        JiraDevCommit {
            id: "abc123def456789".to_string(),
            display_id: "abc123d".to_string(),
            message: "Fix issue PROJ-123".to_string(),
            author: Some("John Doe".to_string()),
            timestamp: Some("2024-01-15T12:42:57.000+0000".to_string()),
            url: "https://github.com/org/repo/commit/abc123d".to_string(),
            file_count: 3,
            merge: false,
        }
    }

    fn sample_branch() -> JiraDevBranch {
        JiraDevBranch {
            name: "feature/new-ui".to_string(),
            url: "https://github.com/org/repo/tree/feature/new-ui".to_string(),
            repository_name: "org/repo".to_string(),
            create_pr_url: Some("https://github.com/org/repo/compare/feature/new-ui".to_string()),
            last_commit: Some(sample_commit()),
        }
    }

    fn sample_repo() -> JiraDevRepository {
        JiraDevRepository {
            name: "org/repo".to_string(),
            url: "https://github.com/org/repo".to_string(),
            commits: vec![sample_commit()],
        }
    }

    // ── DevDataType ───────────────────────────────────────────────

    #[test]
    fn dev_data_type_as_api_str() {
        assert_eq!(DevDataType::Pullrequest.as_api_str(), "pullrequest");
        assert_eq!(DevDataType::Branch.as_api_str(), "branch");
        assert_eq!(DevDataType::Repository.as_api_str(), "repository");
    }

    // ── print helpers ─────────────────────────────────────────────

    #[test]
    fn print_dev_status_empty() {
        let status = JiraDevStatus {
            pull_requests: vec![],
            branches: vec![],
            repositories: vec![],
        };
        print_dev_status("PROJ-1", &status);
    }

    #[test]
    fn print_dev_status_with_prs() {
        let status = JiraDevStatus {
            pull_requests: vec![sample_pr()],
            branches: vec![],
            repositories: vec![],
        };
        print_dev_status("PROJ-1", &status);
    }

    #[test]
    fn print_dev_status_with_branches() {
        let status = JiraDevStatus {
            pull_requests: vec![],
            branches: vec![sample_branch()],
            repositories: vec![],
        };
        print_dev_status("PROJ-1", &status);
    }

    #[test]
    fn print_dev_status_with_repositories() {
        let status = JiraDevStatus {
            pull_requests: vec![],
            branches: vec![],
            repositories: vec![sample_repo()],
        };
        print_dev_status("PROJ-1", &status);
    }

    #[test]
    fn print_dev_status_all_sections() {
        let status = JiraDevStatus {
            pull_requests: vec![sample_pr()],
            branches: vec![sample_branch()],
            repositories: vec![sample_repo()],
        };
        print_dev_status("PROJ-1", &status);
    }

    #[test]
    fn print_summary_empty() {
        let summary = JiraDevStatusSummary {
            pullrequest: JiraDevStatusCount {
                count: 0,
                providers: vec![],
            },
            branch: JiraDevStatusCount {
                count: 0,
                providers: vec![],
            },
            repository: JiraDevStatusCount {
                count: 0,
                providers: vec![],
            },
        };
        print_summary("PROJ-1", &summary);
    }

    #[test]
    fn print_summary_with_data() {
        let summary = JiraDevStatusSummary {
            pullrequest: JiraDevStatusCount {
                count: 2,
                providers: vec!["GitHub".to_string()],
            },
            branch: JiraDevStatusCount {
                count: 1,
                providers: vec!["GitHub".to_string()],
            },
            repository: JiraDevStatusCount {
                count: 1,
                providers: vec!["GitHub".to_string(), "bitbucket".to_string()],
            },
        };
        print_summary("PROJ-1", &summary);
    }

    #[test]
    fn print_commits_table() {
        let commits = vec![sample_commit()];
        print_commits(&commits);
    }

    // ── struct fields ─────────────────────────────────────────────

    #[test]
    fn dev_command_fields() {
        let cmd = DevCommand {
            key: "PROJ-1".to_string(),
            r#type: None,
            app: None,
            summary: false,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.key, "PROJ-1");
        assert!(cmd.r#type.is_none());
        assert!(cmd.app.is_none());
        assert!(!cmd.summary);
    }

    #[test]
    fn dev_command_with_type_filter() {
        let cmd = DevCommand {
            key: "PROJ-1".to_string(),
            r#type: Some(DevDataType::Pullrequest),
            app: None,
            summary: false,
            output: OutputFormat::Json,
        };
        assert_eq!(cmd.key, "PROJ-1");
        assert!(cmd.r#type.is_some());
    }

    #[test]
    fn dev_command_with_app_filter() {
        let cmd = DevCommand {
            key: "PROJ-1".to_string(),
            r#type: None,
            app: Some("GitHub".to_string()),
            summary: false,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.app.as_deref(), Some("GitHub"));
    }

    #[test]
    fn dev_command_summary_mode() {
        let cmd = DevCommand {
            key: "PROJ-1".to_string(),
            r#type: None,
            app: None,
            summary: true,
            output: OutputFormat::Table,
        };
        assert!(cmd.summary);
    }

    #[test]
    fn pr_author_and_reviewers() {
        let pr = sample_pr();
        assert_eq!(pr.author.as_deref(), Some("John Doe"));
        assert_eq!(pr.reviewers, vec!["Jane Smith"]);
        assert_eq!(pr.comment_count, Some(3));
        assert!(pr.last_update.is_some());
    }

    #[test]
    fn commit_fields() {
        let commit = sample_commit();
        assert_eq!(commit.display_id, "abc123d");
        assert_eq!(commit.file_count, 3);
        assert!(!commit.merge);
    }

    #[test]
    fn branch_last_commit() {
        let branch = sample_branch();
        assert!(branch.last_commit.is_some());
        assert!(branch.create_pr_url.is_some());
    }

    #[test]
    fn repo_with_commits() {
        let repo = sample_repo();
        assert_eq!(repo.commits.len(), 1);
        assert_eq!(repo.commits[0].display_id, "abc123d");
    }
}
