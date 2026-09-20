//! `ai jev verify-decision` — checks a decision comment against its cited sources.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::verify::{
    fetch_verify_input, parse_comment_selector, run_verify, Citation, CommentSelector, Source,
    VerifyOptions, DEFAULT_COVERAGE, DEFAULT_REJECT_BELOW, DEFAULT_SUPPORTED,
};
use crate::provider::{Comment, IssueDoc};

use super::common::{format_output, JevFormat};

/// Checks a decision comment against the issues or pull requests it cites.
#[derive(Parser)]
#[command(
    long_about = "Checks a decision comment against the issues or pull requests it cites.\n\n\
Reads the issue's decision comment (by default, its most recent comment that cites another \
issue or pull request), splits it into single factual statements with the configured AI \
backend, and checks each statement against the source it names with Jev — one call per cited \
source, one yes/no question per statement attributed to it — plus one Jev call asking whether \
the split statements together cover everything the comment claims.\n\n\
The verdict is `rejected` if any statement scores below --reject-below, `accepted` if every \
statement clears --threshold and coverage did too, and `needs_review` otherwise: an uncertain \
statement, an unresolved citation, low coverage, or a comment that cites nothing verifiable. \
`verify-decision` checks only that a comment matches its sources; whether the decision is the \
right one remains a person's call."
)]
pub struct VerifyDecisionCommand {
    /// The issue whose decision comment should be checked: `#N` or `N` (in the current
    /// repository), `owner/repo#N`, or an issue URL.
    #[arg(value_name = "ISSUE")]
    pub issue: String,

    /// The comment to check: `latest` (the most recent comment that cites another issue or
    /// pull request), a numeric comment id, or an issue-comment URL.
    #[arg(long, value_name = "ID|latest", default_value = "latest")]
    pub comment: String,

    /// Threshold above which a statement counts as supported.
    #[arg(long = "threshold", value_name = "PROBABILITY", default_value_t = DEFAULT_SUPPORTED)]
    pub threshold: f64,

    /// Threshold below which a statement rejects the comment outright.
    #[arg(long, value_name = "PROBABILITY", default_value_t = DEFAULT_REJECT_BELOW)]
    pub reject_below: f64,

    /// Threshold below which low statement coverage becomes a review reason.
    #[arg(long, value_name = "PROBABILITY", default_value_t = DEFAULT_COVERAGE)]
    pub coverage_threshold: f64,

    /// Longest source text, in characters, sent to Jev; longer text is cut with a marker.
    #[arg(long, value_name = "CHARS", default_value_t = crate::jev::route::DEFAULT_MAX_INPUT_CHARS)]
    pub max_input_chars: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = JevFormat::Json)]
    pub(super) output: JevFormat,

    /// Overrides the configured Jev model for this call.
    #[arg(long, value_name = "MODEL")]
    pub jev_model: Option<String>,

    /// `-C/--repo`: the repository that `#N` and a cited `#M` resolve against.
    #[command(flatten)]
    pub repo: crate::cli::repo_arg::RepoArg,

    /// AI backend selection (`--ai-backend`, `--model`, …), used to split the decision
    /// comment into statements — the one thing about this command that is a chat completion,
    /// not a Jev call.
    #[command(flatten)]
    pub ai: crate::cli::ai_backend_args::AiBackendArgs,
}

impl VerifyDecisionCommand {
    /// Executes the verify-decision command.
    pub async fn execute(self) -> Result<()> {
        self.ai.apply();

        let mut jev_config = JevConfig::from_env()?;
        if let Some(model) = self.jev_model {
            jev_config.model = model;
        }
        let jev = JevClient::from_config(&jev_config)?;

        crate::utils::preflight::check_ai_credentials(None)?;
        let ai = crate::claude::create_default_claude_client(None, None).await?;

        let selector = parse_comment_selector(&self.comment)?;
        let bin = crate::pr_status::resolve_gh_binary();
        let cwd = self
            .repo
            .path()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let issue_arg = self.issue;

        let (issue, comment, citations, sources) =
            tokio::task::spawn_blocking(move || fetch_input(&bin, &cwd, &issue_arg, &selector))
                .await
                .context("Issue fetch task panicked")??;

        let opts = VerifyOptions {
            jev_model: jev_config.model,
            supported: self.threshold,
            reject_below: self.reject_below,
            coverage: self.coverage_threshold,
            max_input_chars: self.max_input_chars,
        };
        let report = run_verify(&jev, &ai, &issue, &comment, &citations, &sources, &opts).await?;
        print!("{}", format_output(&report, self.output)?);
        Ok(())
    }
}

/// Resolves `<ISSUE>` and fetches everything `verify-decision` needs.
/// **Blocking** — callers must be on a blocking thread.
fn fetch_input(
    bin: &Path,
    cwd: &Path,
    issue_arg: &str,
    selector: &CommentSelector,
) -> Result<(IssueDoc, Comment, Vec<Citation>, Vec<Source>)> {
    let default_project = if crate::github_issues::needs_default_project(issue_arg) {
        Some(
            crate::github_issues::resolve_current_project(bin, cwd).context(
                "Failed to find the current GitHub repository; pass owner/repo#N or use -C/--repo",
            )?,
        )
    } else {
        None
    };
    let judged = crate::github_issues::parse_issue_arg(issue_arg, default_project.as_deref())?;
    let default_project = default_project.unwrap_or_else(|| judged.project.clone());
    fetch_verify_input(bin, &default_project, &judged, selector)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::test_support::shim::{retry_on_etxtbsy, shim_lock, write_exec_script};
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> Result<VerifyDecisionCommand, clap::Error> {
        let argv = ["omni-dev", "ai", "jev", "verify-decision"]
            .iter()
            .chain(args)
            .copied();
        let cli = Cli::try_parse_from(argv)?;
        let crate::cli::Commands::Ai(ai) = cli.command else {
            panic!("expected ai");
        };
        let crate::cli::ai::AiSubcommands::Jev(jev) = ai.command else {
            panic!("expected ai jev");
        };
        let super::super::JevSubcommands::VerifyDecision(cmd) = jev.command else {
            panic!("expected ai jev verify-decision");
        };
        Ok(cmd)
    }

    #[test]
    fn parses_issue_and_defaults() {
        let cmd = parse(&["#1779"]).unwrap();
        assert_eq!(cmd.issue, "#1779");
        assert_eq!(cmd.comment, "latest");
        assert!((cmd.threshold - DEFAULT_SUPPORTED).abs() < f64::EPSILON);
        assert!((cmd.reject_below - DEFAULT_REJECT_BELOW).abs() < f64::EPSILON);
        assert!((cmd.coverage_threshold - DEFAULT_COVERAGE).abs() < f64::EPSILON);
        assert_eq!(cmd.output, JevFormat::Json);
    }

    #[test]
    fn accepts_repo_and_ai_backend_flags() {
        let cmd = parse(&[
            "#1779",
            "-C",
            "/tmp/r",
            "--ai-backend",
            "claude-cli",
            "--model",
            "opus",
            "--comment",
            "12345",
            "--threshold",
            "0.6",
        ])
        .unwrap();
        assert_eq!(cmd.repo.path(), Some(Path::new("/tmp/r")));
        assert_eq!(
            cmd.ai.ai_backend,
            Some(crate::claude::backend::AiBackend::ClaudeCli)
        );
        assert_eq!(cmd.comment, "12345");
    }

    #[test]
    fn requires_an_issue_argument() {
        assert!(parse(&[]).is_err());
    }

    // ── fetch_input (fake-gh shim) ────────────────────────────────────

    fn fake_gh(dir: &Path) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let issue = serde_json::json!({
            "title": "t", "body": "settled by #1614", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 1, "nodes": [
                {"databaseId": 1, "author": {"login": "newhoggy"}, "body": "settled by #1614"}
            ]},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let cited = serde_json::json!({
            "__typename": "Issue",
            "title": "cited", "body": "cited body", "state": "CLOSED", "url": "u2",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        // Two distinct `gh api graphql` calls happen: `fetch_issues` for the
        // judged issue (query uses `issue(number:`) and `fetch_items` for its
        // citation (query uses the polymorphic `issueOrPullRequest(number:`),
        // each aliasing its one item as `r0`/`i0` — so the shim must branch on
        // the query text ($4), not just on the subcommand.
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 repo) echo rust-works/omni-dev ;;\n\
                 api) case \"$4\" in\n\
                 *issueOrPullRequest*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {cited}}}}}}}\nJSON\n;;\n\
                 *) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {issue}}}}}}}\nJSON\n;;\n\
                 esac ;;\n\
                 esac\n",
            ),
        );
        (path, guard)
    }

    #[test]
    fn fetch_input_resolves_the_issue_and_its_citation() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let (issue, comment, citations, sources) =
            retry_on_etxtbsy(|| fetch_input(&bin, dir.path(), "#1", &CommentSelector::Latest))
                .unwrap();
        assert_eq!(issue.number, 1);
        assert_eq!(comment.body, "settled by #1614");
        assert_eq!(citations.len(), 1);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].label, "#1614");
    }

    /// An `owner/repo#N` issue argument already names its project, so
    /// `fetch_input` never needs to shell out to `gh repo view` for it.
    #[test]
    fn fetch_input_skips_project_resolution_when_the_issue_names_its_own_repo() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let (issue, comment, citations, sources) = retry_on_etxtbsy(|| {
            fetch_input(
                &bin,
                dir.path(),
                "rust-works/omni-dev#1",
                &CommentSelector::Latest,
            )
        })
        .unwrap();
        assert_eq!(issue.number, 1);
        assert_eq!(comment.body, "settled by #1614");
        assert_eq!(citations.len(), 1);
        assert_eq!(sources.len(), 1);
    }
}
