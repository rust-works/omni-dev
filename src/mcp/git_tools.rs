//! MCP tool handlers for git operations.
//!
//! Each handler delegates to the same `run_*` function that the CLI uses, so
//! the MCP surface and the CLI share a single implementation.

use std::path::PathBuf;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::cancel::spawn_blocking_cancellable;
use super::error::tool_error;
use super::server::OmniDevServer;
use super::truncate::{truncate_response, DEFAULT_MAX_RESPONSE_BYTES};
use super::validate::{validate_range, validate_repo_path};

/// Parameters for the `git_view_commits` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitViewCommitsParams {
    /// Commit range to analyze (e.g., `HEAD~3..HEAD`, `abc123..def456`).
    /// Defaults to `HEAD` when omitted.
    #[serde(default)]
    pub range: Option<String>,
    /// Path to the git repository. Must be absolute when provided.
    /// Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}

/// Parameters for the `git_branch_info` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitBranchInfoParams {
    /// Base branch to compare against. Defaults to `main` or `master`.
    #[serde(default)]
    pub branch: Option<String>,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}

/// Parameters for the `git_check_commits` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitCheckCommitsParams {
    /// Commit range to check (e.g., `HEAD~3..HEAD`, `abc123..def456`).
    pub range: String,
    /// Optional explicit path to the guidelines file. When omitted the tool
    /// falls back to `.omni-dev/commit-guidelines.md` via the standard
    /// resolution chain.
    #[serde(default)]
    pub guidelines_path: Option<String>,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
    /// When true, warnings are treated as non-zero exit conditions.
    #[serde(default)]
    pub strict: bool,
    /// Claude model override.
    #[serde(default)]
    pub model: Option<String>,
}

/// Parameters for the `git_twiddle_commits` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitTwiddleCommitsParams {
    /// Commit range to twiddle. Defaults to `HEAD~5..HEAD` when omitted.
    #[serde(default)]
    pub range: Option<String>,
    /// Claude model override.
    #[serde(default)]
    pub model: Option<String>,
    /// When true, proposed amendments are returned without being applied.
    /// When false (or omitted), amendments are applied automatically — the
    /// MCP boundary is non-interactive and therefore forces `--auto-apply`
    /// semantics; no editor is started.
    #[serde(default)]
    pub dry_run: bool,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}

/// Parameters for the `git_staged_commit` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitStagedCommitParams {
    /// When true, the generated commit message is returned without being
    /// committed to the repository. Defaults to `false` (commit applied).
    #[serde(default)]
    pub print_only: bool,
    /// Claude model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}

/// Parameters for the `git_create_pr` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GitCreatePrParams {
    /// Claude model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Base branch for the PR. Defaults to the primary remote's main branch.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `git_tool_router` fn.
#[tool_router(router = git_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: analyse commits in a range and return repository information as YAML.
    #[tool(
        description = "Analyze commits in a range and return repository information as YAML. \
                       Mirrors `omni-dev git commit message view`."
    )]
    pub async fn git_view_commits(
        &self,
        Parameters(params): Parameters<GitViewCommitsParams>,
        cancellation: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let range = params.range.as_deref().unwrap_or("HEAD").to_string();
        validate_range(&range)?;
        let repo_path = params.repo_path.clone();
        repo_path.as_deref().map(validate_repo_path).transpose()?;

        tracing::debug!(
            tool = "git_view_commits",
            range = %range,
            repo_path = ?repo_path,
            "invoking tool"
        );

        let range_for_task = range.clone();
        let yaml = spawn_blocking_cancellable(&cancellation, move || {
            crate::cli::git::run_view(&range_for_task, repo_path.as_deref())
        })
        .await?;

        Ok(build_truncated_result(yaml))
    }

    /// Tool: analyse branch commits and return repository info as YAML.
    #[tool(
        description = "Analyze branch commits against a base and return repository information \
                       as YAML. Mirrors `omni-dev git branch info`."
    )]
    pub async fn git_branch_info(
        &self,
        Parameters(params): Parameters<GitBranchInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let branch = params.branch.clone();
        let repo_path = params.repo_path.clone();

        let yaml = tokio::task::spawn_blocking(move || {
            crate::cli::git::run_info(branch.as_deref(), repo_path.as_deref())
        })
        .await
        .map_err(|e| tool_error(anyhow::anyhow!("join error: {e}")))?
        .map_err(tool_error)?;

        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }

    /// Tool: validate commit messages against guidelines.
    #[tool(
        description = "Validate commit messages in a range against commit guidelines. \
                       Mirrors `omni-dev git commit message check`. Returns a YAML payload with \
                       the full CheckReport, a pass/fail summary, and the exit code the CLI \
                       would use (honouring `strict`)."
    )]
    pub async fn git_check_commits(
        &self,
        Parameters(params): Parameters<GitCheckCommitsParams>,
    ) -> Result<CallToolResult, McpError> {
        let outcome = crate::cli::git::run_check(
            &params.range,
            params.guidelines_path.as_deref().map(std::path::Path::new),
            params.repo_path.as_deref().map(std::path::Path::new),
            params.strict,
            params.model,
        )
        .await
        .map_err(tool_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            format_check_payload(&outcome),
        )]))
    }

    /// Tool: AI-powered commit message improvement.
    #[tool(
        description = "Generate improved commit messages for a range and (by default) apply \
                       them. Mirrors `omni-dev git commit message twiddle --auto-apply`. Set \
                       `dry_run = true` to return the proposed amendments as YAML without \
                       applying them. The editor is never started from this tool."
    )]
    pub async fn git_twiddle_commits(
        &self,
        Parameters(params): Parameters<GitTwiddleCommitsParams>,
    ) -> Result<CallToolResult, McpError> {
        let range = params.range.clone();
        let model = params.model.clone();
        let dry_run = params.dry_run;
        let repo_path: Option<PathBuf> = params.repo_path.as_deref().map(PathBuf::from);

        let outcome =
            crate::cli::git::run_twiddle(range.as_deref(), model, dry_run, repo_path.as_deref())
                .await
                .map_err(tool_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            format_twiddle_payload(&outcome, dry_run),
        )]))
    }

    /// Tool: generate a commit message from staged changes and commit them.
    #[tool(
        description = "Generate a Conventional Commits message from the currently staged diff \
                       and (by default) commit it via `git commit -m`. Mirrors \
                       `omni-dev git commit message staged`. Set `print_only = true` to return \
                       the generated message without committing."
    )]
    pub async fn git_staged_commit(
        &self,
        Parameters(params): Parameters<GitStagedCommitParams>,
    ) -> Result<CallToolResult, McpError> {
        let print_only = params.print_only;
        let model = params.model.clone();
        params
            .repo_path
            .as_deref()
            .map(validate_repo_path)
            .transpose()?;
        let repo_path: Option<PathBuf> = params.repo_path.as_deref().map(PathBuf::from);

        let outcome =
            crate::cli::git::run_staged(print_only, model, None, None, repo_path.as_deref())
                .await
                .map_err(tool_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            format_staged_payload(&outcome, print_only),
        )]))
    }

    /// Tool: generate a PR title + description via the AI.
    #[tool(
        description = "Generate an AI-drafted pull request title and description for the \
                       current branch. Mirrors `omni-dev git branch create pr` in its \
                       content-generation phase — this tool returns the proposed PR content as \
                       YAML and does NOT push the branch or invoke `gh pr create`."
    )]
    pub async fn git_create_pr(
        &self,
        Parameters(params): Parameters<GitCreatePrParams>,
    ) -> Result<CallToolResult, McpError> {
        let model = params.model.clone();
        let base_branch = params.base_branch.clone();
        let repo_path: Option<PathBuf> = params.repo_path.as_deref().map(PathBuf::from);

        let outcome =
            crate::cli::git::run_create_pr(model, base_branch.as_deref(), repo_path.as_deref())
                .await
                .map_err(tool_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            outcome.pr_yaml,
        )]))
    }
}

/// Wraps a text result in a `CallToolResult`, applying the default response
/// cap and emitting a second `Content::text` payload carrying a JSON
/// `{"truncated": bool, "original_bytes": usize}` marker when truncation
/// happened.
///
/// Shared by every tool that can produce large output so the truncation
/// contract is consistent across the MCP surface.
pub(crate) fn build_truncated_result(text: String) -> CallToolResult {
    let original_bytes = text.len();
    let (body, truncated) = truncate_response(text, DEFAULT_MAX_RESPONSE_BYTES);
    if truncated {
        let marker = json!({
            "truncated": true,
            "original_bytes": original_bytes,
            "limit_bytes": DEFAULT_MAX_RESPONSE_BYTES,
        });
        CallToolResult::success(vec![Content::text(body), Content::text(marker.to_string())])
    } else {
        CallToolResult::success(vec![Content::text(body)])
    }
}

/// Indents a multi-line string for inclusion as a YAML block scalar value.
fn indent_for_yaml(body: &str) -> String {
    body.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Formats the payload returned by the `git_check_commits` tool.
fn format_check_payload(outcome: &crate::cli::git::CheckOutcome) -> String {
    format!(
        "# git_check_commits outcome\nexit_code: {}\nstrict: {}\nhas_errors: {}\nhas_warnings: {}\ntotal_commits: {}\nreport: |\n{}",
        outcome.exit_code,
        outcome.strict,
        outcome.has_errors,
        outcome.has_warnings,
        outcome.total_commits,
        indent_for_yaml(&outcome.report_yaml),
    )
}

/// Formats the payload returned by the `git_twiddle_commits` tool.
fn format_twiddle_payload(outcome: &crate::cli::git::TwiddleOutcome, dry_run: bool) -> String {
    format!(
        "# git_twiddle_commits outcome\napplied: {}\ndry_run: {}\namendment_count: {}\namendments: |\n{}",
        outcome.applied,
        dry_run,
        outcome.amendment_count,
        indent_for_yaml(&outcome.amendments_yaml),
    )
}

/// Formats the payload returned by the `git_staged_commit` tool.
fn format_staged_payload(outcome: &crate::cli::git::StagedOutcome, print_only: bool) -> String {
    format!(
        "# git_staged_commit outcome\napplied: {}\nprint_only: {}\nmessage: |\n{}",
        outcome.applied,
        print_only,
        indent_for_yaml(&outcome.message),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::git::{CheckOutcome, StagedOutcome, TwiddleOutcome};

    #[test]
    fn build_truncated_result_leaves_small_output_alone() {
        let result = build_truncated_result("hello".to_string());
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn build_truncated_result_appends_marker_when_over_cap() {
        let big = "x".repeat(DEFAULT_MAX_RESPONSE_BYTES + 1024);
        let result = build_truncated_result(big);
        assert_eq!(result.content.len(), 2, "expected body + truncation marker");
        let marker_raw = result.content[1]
            .as_text()
            .expect("second payload should be text")
            .text
            .clone();
        let parsed: serde_json::Value = serde_json::from_str(&marker_raw).expect("marker is JSON");
        assert_eq!(parsed["truncated"], serde_json::Value::Bool(true));
        let original = parsed["original_bytes"].as_u64().unwrap();
        let limit = parsed["limit_bytes"].as_u64().unwrap();
        assert!(original > limit);
    }

    #[test]
    fn indent_for_yaml_empty_string() {
        assert_eq!(indent_for_yaml(""), "");
    }

    #[test]
    fn indent_for_yaml_single_line() {
        assert_eq!(indent_for_yaml("hello"), "  hello");
    }

    #[test]
    fn indent_for_yaml_multi_line() {
        let input = "line1\nline2\nline3";
        let expected = "  line1\n  line2\n  line3";
        assert_eq!(indent_for_yaml(input), expected);
    }

    #[test]
    fn format_check_payload_includes_all_fields() {
        let outcome = CheckOutcome {
            report_yaml: "checks:\n  - commit: abc\n".to_string(),
            has_errors: true,
            has_warnings: false,
            total_commits: 3,
            strict: true,
            exit_code: 1,
        };
        let payload = format_check_payload(&outcome);
        assert!(payload.contains("exit_code: 1"));
        assert!(payload.contains("strict: true"));
        assert!(payload.contains("has_errors: true"));
        assert!(payload.contains("has_warnings: false"));
        assert!(payload.contains("total_commits: 3"));
        assert!(payload.contains("  checks:"), "report should be indented");
    }

    #[test]
    fn format_check_payload_clean_outcome() {
        let outcome = CheckOutcome {
            report_yaml: String::new(),
            has_errors: false,
            has_warnings: false,
            total_commits: 0,
            strict: false,
            exit_code: 0,
        };
        let payload = format_check_payload(&outcome);
        assert!(payload.contains("exit_code: 0"));
        assert!(payload.contains("strict: false"));
    }

    #[test]
    fn format_twiddle_payload_applied() {
        let outcome = TwiddleOutcome {
            amendments_yaml: "amendments:\n  - commit: abc\n".to_string(),
            applied: true,
            amendment_count: 2,
        };
        let payload = format_twiddle_payload(&outcome, false);
        assert!(payload.contains("applied: true"));
        assert!(payload.contains("dry_run: false"));
        assert!(payload.contains("amendment_count: 2"));
        assert!(payload.contains("  amendments:"));
    }

    #[test]
    fn format_twiddle_payload_dry_run_not_applied() {
        let outcome = TwiddleOutcome {
            amendments_yaml: "amendments: []\n".to_string(),
            applied: false,
            amendment_count: 0,
        };
        let payload = format_twiddle_payload(&outcome, true);
        assert!(payload.contains("applied: false"));
        assert!(payload.contains("dry_run: true"));
        assert!(payload.contains("amendment_count: 0"));
    }

    #[test]
    fn format_staged_payload_applied() {
        let outcome = StagedOutcome {
            message: "feat(cli): add staged subcommand".to_string(),
            applied: true,
        };
        let payload = format_staged_payload(&outcome, false);
        assert!(payload.contains("applied: true"));
        assert!(payload.contains("print_only: false"));
        assert!(payload.contains("  feat(cli): add staged subcommand"));
    }

    #[test]
    fn format_staged_payload_print_only() {
        let outcome = StagedOutcome {
            message: "fix(x): y\n\nBody.".to_string(),
            applied: false,
        };
        let payload = format_staged_payload(&outcome, true);
        assert!(payload.contains("applied: false"));
        assert!(payload.contains("print_only: true"));
        assert!(payload.contains("  fix(x): y"));
        assert!(payload.contains("  Body."));
    }

    // Direct MCP handler invocation — exercises parameter destructuring and
    // error wrapping without needing a full duplex client/server pair.

    #[tokio::test]
    async fn git_branch_info_handler_invalid_repo_path_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = GitBranchInfoParams {
            branch: None,
            repo_path: Some("/no/such/path/for/mcp/test".to_string()),
        };
        let err = server
            .git_branch_info(Parameters(params))
            .await
            .unwrap_err();
        assert!(!err.message.is_empty(), "expected non-empty error message");
    }

    #[tokio::test]
    async fn git_check_commits_handler_invalid_repo_path_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = GitCheckCommitsParams {
            range: "HEAD".to_string(),
            guidelines_path: None,
            repo_path: Some("/no/such/path/for/mcp/test".to_string()),
            strict: false,
            model: None,
        };
        let err = server
            .git_check_commits(Parameters(params))
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn git_twiddle_commits_handler_invalid_repo_path_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = GitTwiddleCommitsParams {
            range: None,
            model: None,
            dry_run: true,
            repo_path: Some("/no/such/path/for/mcp/test".to_string()),
        };
        let err = server
            .git_twiddle_commits(Parameters(params))
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn git_create_pr_handler_invalid_repo_path_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;

        let server = OmniDevServer::new();
        let params = GitCreatePrParams {
            model: None,
            base_branch: None,
            repo_path: Some("/no/such/path/for/mcp/test".to_string()),
        };
        let err = server.git_create_pr(Parameters(params)).await.unwrap_err();
        assert!(!err.message.is_empty());
    }
}
