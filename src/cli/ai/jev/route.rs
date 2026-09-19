//! `ai jev route` — routes issues to model classes by stage.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::github_issues::{
    fetch_issues, list_open_issue_numbers, needs_default_project, parse_issue_arg,
    resolve_current_project,
};
use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::route::{
    run_route, RouteOptions, RouteReport, Tiers, DEFAULT_CLOSE_CALL, DEFAULT_MAX_INPUT_CHARS,
};
use crate::provider::{GitProvider, IssueDoc, ItemKind, ItemRef};

use super::common::{format_output, JevFormat};

/// Routes issues to model classes for their design, implement and review stages.
#[derive(Parser)]
#[command(
    long_about = "Routes issues to model classes for their design, implement and review \
stages.\n\nMakes one Jev call per issue. Jev judges the issue's title, body and human \
comments (never the issues or pull requests it references) and picks, for each stage, the \
least capable class likely to do it correctly with no rework. The issue's class is the \
higher of its design and implement choices, and a stage whose confidence is below \
--close-call is listed under close_calls.\n\nClosed issues are refused unless --allow-closed \
is given: their comments often describe how the work was done, which leaks the answer.\n\n\
An issue whose Jev call fails is reported with an `error` field instead of stages, and the \
rest are still routed; the command then exits non-zero. An authentication failure stops the \
run at once."
)]
pub struct RouteCommand {
    /// Issues to route: `#N` or `N` (in the current repository), `owner/repo#N`, or an
    /// issue URL.
    #[arg(
        value_name = "ISSUE",
        required_unless_present = "all_open",
        conflicts_with = "all_open"
    )]
    pub issues: Vec<String>,

    /// Routes every open issue in the current repository.
    #[arg(long)]
    pub all_open: bool,

    /// YAML file of model-class tiers, least capable first, replacing the defaults.
    #[arg(long, value_name = "FILE")]
    pub tiers: Option<PathBuf>,

    /// Confidence below which a stage is reported as a close call.
    #[arg(long, value_name = "CONFIDENCE", default_value_t = DEFAULT_CLOSE_CALL)]
    pub close_call: f64,

    /// Longest issue text, in characters, sent to Jev; longer text is cut with a marker.
    #[arg(long, value_name = "CHARS", default_value_t = DEFAULT_MAX_INPUT_CHARS)]
    pub max_input_chars: usize,

    /// Routes closed issues instead of refusing them.
    #[arg(long)]
    pub allow_closed: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = JevFormat::Json)]
    pub(super) output: JevFormat,

    /// Overrides the configured Jev model for this call.
    #[arg(long, value_name = "MODEL")]
    pub jev_model: Option<String>,

    /// `-C/--repo`: the repository that `#N` and `--all-open` resolve against.
    #[command(flatten)]
    pub repo: crate::cli::repo_arg::RepoArg,
}

impl RouteCommand {
    /// Executes the route command.
    pub async fn execute(self) -> Result<()> {
        let mut config = JevConfig::from_env()?;
        if let Some(model) = self.jev_model {
            config.model = model;
        }
        let tiers = Tiers::load(self.tiers.as_deref())?;
        let client = JevClient::from_config(&config)?;

        let bin = crate::pr_status::resolve_gh_binary();
        let cwd = self
            .repo
            .path()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let (issues, all_open) = (self.issues, self.all_open);
        let docs = tokio::task::spawn_blocking(move || fetch_docs(&bin, &cwd, &issues, all_open))
            .await
            .context("Issue fetch task panicked")??;

        let opts = RouteOptions {
            model: config.model,
            close_call: self.close_call,
            max_input_chars: self.max_input_chars,
            allow_closed: self.allow_closed,
        };
        let report = run_route(&client, &docs, &tiers, &opts).await?;
        print!("{}", format_output(&report, self.output)?);
        failure_summary(&report).map_or(Ok(()), |msg| bail!(msg))
    }
}

/// The exit error for a report with failed issues, after the report itself
/// has been printed: the answers that did come back are kept, but a script
/// still sees a non-zero exit.
fn failure_summary(report: &RouteReport) -> Option<String> {
    let failed = report.issues.iter().filter(|i| i.failed()).count();
    (failed > 0).then(|| {
        format!(
            "{failed} of {} issues could not be routed; see their `error` fields",
            report.issues.len()
        )
    })
}

/// Resolves the `<ISSUE>` arguments (or `--all-open`) to issue references and
/// fetches them, in order, without duplicates. The current repository is
/// looked up through `gh` only when an argument needs it. **Blocking.**
fn fetch_docs(bin: &Path, cwd: &Path, issues: &[String], all_open: bool) -> Result<Vec<IssueDoc>> {
    let default_project = if all_open || issues.iter().any(|a| needs_default_project(a)) {
        Some(resolve_current_project(bin, cwd).context(
            "Failed to find the current GitHub repository; pass owner/repo#N or use -C/--repo",
        )?)
    } else {
        None
    };

    let refs = match (all_open, default_project.as_deref()) {
        (true, Some(project)) => {
            let refs: Vec<ItemRef> = list_open_issue_numbers(bin, project)?
                .into_iter()
                .map(|number| ItemRef {
                    provider: GitProvider::GitHub,
                    project: project.to_string(),
                    kind: ItemKind::Issue,
                    number,
                })
                .collect();
            if refs.is_empty() {
                bail!("{project} has no open issues");
            }
            refs
        }
        _ => issues
            .iter()
            .map(|arg| parse_issue_arg(arg, default_project.as_deref()))
            .collect::<Result<Vec<_>>>()?,
    };

    let mut seen = HashSet::new();
    let refs: Vec<ItemRef> = refs
        .into_iter()
        .filter(|r| seen.insert((r.project.clone(), r.number)))
        .collect();
    fetch_issues(bin, &refs)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::test_support::shim::{retry_on_etxtbsy, shim_lock, write_exec_script};

    fn parse(args: &[&str]) -> Result<RouteCommand, clap::Error> {
        let argv = ["omni-dev", "ai", "jev", "route"]
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
        let super::super::JevSubcommands::Route(route) = jev.command else {
            panic!("expected ai jev route");
        };
        Ok(route)
    }

    #[test]
    fn route_parses_issues_and_defaults() {
        let cmd = parse(&["#12", "owner/repo#3"]).unwrap();
        assert_eq!(cmd.issues, ["#12", "owner/repo#3"]);
        assert!((cmd.close_call - DEFAULT_CLOSE_CALL).abs() < f64::EPSILON);
        assert_eq!(cmd.max_input_chars, DEFAULT_MAX_INPUT_CHARS);
        assert!(!cmd.allow_closed);
        assert_eq!(cmd.output, JevFormat::Json);
    }

    #[test]
    fn route_needs_issues_or_all_open_but_not_both() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--all-open"]).is_ok());
        assert!(parse(&["--all-open", "#1"]).is_err());
    }

    #[test]
    fn route_accepts_repo_after_the_leaf() {
        let cmd = parse(&["#1", "-C", "/tmp/r"]).unwrap();
        assert_eq!(cmd.repo.path(), Some(Path::new("/tmp/r")));
    }

    #[test]
    fn route_rejects_ai_backend_flags() {
        assert!(parse(&["#1", "--model", "x"]).is_err());
        assert!(parse(&["#1", "--ai-backend", "ollama"]).is_err());
    }

    #[test]
    fn failure_summary_counts_only_failed_issues() {
        use crate::jev::protocol::Usage;
        use crate::jev::route::{IssueRoute, RouteOutcome, StageAnswer, StageAnswers};
        let answer = || StageAnswer {
            choice: "sonnet".to_string(),
            confidence: 0.9,
            probabilities: std::collections::BTreeMap::new(),
        };
        let issue = |outcome| IssueRoute {
            item_ref: "o/r#1".to_string(),
            url: String::new(),
            title: String::new(),
            outcome,
            truncated: false,
        };
        let routed = || {
            issue(RouteOutcome::Routed {
                stages: StageAnswers {
                    design: answer(),
                    implement: answer(),
                    review: answer(),
                },
                class: "sonnet".to_string(),
                close_calls: vec![],
            })
        };
        let failed = || {
            issue(RouteOutcome::Failed {
                error: "HTTP 529".to_string(),
            })
        };
        let report = |issues| RouteReport {
            model: String::new(),
            issues,
            usage: Usage::default(),
        };

        assert_eq!(failure_summary(&report(vec![routed(), routed()])), None);
        let msg = failure_summary(&report(vec![routed(), failed(), routed()])).unwrap();
        assert!(msg.starts_with("1 of 3 issues"), "{msg}");
    }

    // ── fetch_docs (fake-gh shim) ────────────────────────────────────

    /// A fake `gh` answering `repo view`, `issue list` and `api graphql`,
    /// logging each invocation's first argument to `calls`.
    fn fake_gh(dir: &Path) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let issue = serde_json::json!({
            "title": "t", "body": "b", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let graphql = serde_json::json!({"data": {"r0": {"i0": issue, "i1": issue}}});
        let path = dir.join("fake-gh");
        let calls = dir.join("calls");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\necho \"$1\" >> '{calls}'\ncase \"$1\" in\n\
                 repo) echo rust-works/omni-dev ;;\n\
                 issue) echo '[{{\"number\": 5}}, {{\"number\": 6}}]' ;;\n\
                 api) cat <<'JSON'\n{graphql}\nJSON\n;;\n\
                 esac\n",
                calls = calls.display()
            ),
        );
        (path, guard)
    }

    fn calls(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn fetch_docs_skips_repo_lookup_for_qualified_refs() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let docs = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                dir.path(),
                &["rust-works/omni-dev#1".to_string()],
                false,
            )
        })
        .unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(calls(dir.path()), ["api"]);
    }

    #[test]
    fn fetch_docs_resolves_bare_numbers_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let args = ["#1", "1", "rust-works/omni-dev#2"].map(str::to_string);
        let docs = retry_on_etxtbsy(|| fetch_docs(&bin, dir.path(), &args, false)).unwrap();
        let numbers: Vec<u64> = docs.iter().map(|d| d.number).collect();
        assert_eq!(numbers, [1, 2]);
        assert_eq!(calls(dir.path()), ["repo", "api"]);
    }

    #[test]
    fn fetch_docs_all_open_lists_the_current_repository() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let docs = retry_on_etxtbsy(|| fetch_docs(&bin, dir.path(), &[], true)).unwrap();
        let numbers: Vec<u64> = docs.iter().map(|d| d.number).collect();
        assert_eq!(numbers, [5, 6]);
        assert_eq!(calls(dir.path()), ["repo", "issue", "api"]);
    }

    #[test]
    fn fetch_docs_explains_a_failed_repo_lookup() {
        let err = fetch_docs(
            Path::new("/no/such/gh/xyzzy"),
            Path::new("."),
            &["#1".to_string()],
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("-C/--repo"), "{err}");
    }
}
