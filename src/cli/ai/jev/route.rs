//! `ai jev route` — routes issues to model classes by stage.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::github_issues::{
    fetch_issues, fetch_items, list_open_issue_numbers, needs_default_project, parse_issue_arg,
    resolve_current_project,
};
use crate::jev::citations::{find_citations, Citation};
use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::route::{
    build_route_state, render_route_text, run_route, Ladder, OpenDependencies, Provider,
    RouteOptions, RouteReport, Tiers, DEFAULT_CLOSE_CALL, DEFAULT_MAX_INPUT_CHARS,
};
use crate::provider::{GitProvider, IssueDoc, ItemKind, ItemRef, ItemState};

use super::common::{format_output, JevFormat};

/// Output format for `route`.
///
/// Route-only, not a variant of the shared [`JevFormat`]: `route`'s
/// stage/provider breakdown is the only `ai jev` leaf with a natural human
/// paragraph to write, so the other leaves (`choice`/`score`/`noul`/`ask`/
/// `verify-decision`) never have to reject a `Text` variant that would never
/// apply to them.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RouteFormat {
    /// Pretty-printed JSON (default).
    #[default]
    Json,
    /// YAML.
    Yaml,
    /// One human-readable paragraph per issue. The wording is not a stable
    /// contract and may change without notice; scripts should use `json` or
    /// `yaml`.
    Text,
}

/// Routes issues to model classes for their design, implement and review stages.
#[derive(Parser)]
#[command(
    long_about = "Routes issues to model classes for their design, implement and review \
stages.\n\nMakes one Jev call per issue. Jev judges the issue's title, body and human \
comments (never the issues or pull requests it references) and picks, for each stage, the \
least capable class likely to do it correctly with no rework. The issue's class is the \
higher of its design and implement choices, and a stage whose confidence is below \
--close-call is listed under close_calls.\n\nThe classes are one AI provider's model ladder, \
named by abbreviated model name: anthropic (sonnet/opus/fable, the default), openai \
(terra/sol/astra) and gemini (flash/pro/deep-think). --providers routes against several at \
once, still in one Jev call per issue, and the output nests stages, class and close_calls \
under each provider's name. --tiers FILE swaps in a custom ladder instead, reported under \
`custom`; it cannot be combined with --providers.\n\nEach open issue or pull request the text \
cites is reported under depends_on, with a could_be_cheaper.design probability: how likely it \
is that resolving that dependency would leave less design work remaining than the text \
implies. One extra Jev question is asked per open citation; a closed citation is settled and \
is not reported.\n\nClosed issues are refused unless --allow-closed is given: their comments \
often describe how the work was done, which leaks the answer.\n\nAn issue whose Jev call \
fails is reported with an `error` field instead of providers, and the rest are still routed; \
the command then exits non-zero. An authentication failure stops the run at once."
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

    /// AI providers whose model ladders to route against (comma-separated).
    #[arg(
        long,
        value_name = "NAMES",
        value_enum,
        value_delimiter = ',',
        default_value = "anthropic",
        conflicts_with = "tiers"
    )]
    pub providers: Vec<Provider>,

    /// YAML file of model-class tiers, least capable first, replacing the provider ladders.
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
    #[arg(short = 'o', long, value_enum, default_value_t = RouteFormat::Json)]
    pub(super) output: RouteFormat,

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
        let ladders = build_ladders(self.tiers.as_deref(), &self.providers)?;
        let client = JevClient::from_config(&config)?;

        let bin = crate::pr_status::resolve_gh_binary();
        let cwd = self
            .repo
            .path()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let (issues, all_open, max_input_chars) =
            (self.issues, self.all_open, self.max_input_chars);
        let (docs, dependencies) = tokio::task::spawn_blocking(move || {
            fetch_docs(&bin, &cwd, &issues, all_open, max_input_chars)
        })
        .await
        .context("Issue fetch task panicked")??;

        let opts = RouteOptions {
            model: config.model,
            close_call: self.close_call,
            max_input_chars: self.max_input_chars,
            allow_closed: self.allow_closed,
        };
        let report = run_route(&client, &docs, &ladders, &opts, &dependencies).await?;
        print!(
            "{}",
            render_output(&report, self.output, self.max_input_chars)?
        );
        failure_summary(&report).map_or(Ok(()), |msg| bail!(msg))
    }
}

/// Renders a routed report in `output`'s format. Split out of
/// [`RouteCommand::execute`] so the format dispatch is unit-testable without
/// a Jev client or a `gh` binary — `execute` itself stays an untested thin
/// shell, per the project's `run_*` convention (STYLE-0025).
fn render_output(
    report: &RouteReport,
    output: RouteFormat,
    max_input_chars: usize,
) -> Result<String> {
    Ok(match output {
        RouteFormat::Json => format_output(report, JevFormat::Json)?,
        RouteFormat::Yaml => format_output(report, JevFormat::Yaml)?,
        RouteFormat::Text => render_route_text(report, max_input_chars),
    })
}

/// Resolves `--tiers`/`--providers` to the ladders to route against: a
/// single custom ladder for `--tiers`, or one builtin ladder per
/// `--providers` entry (in the order given) otherwise. Clap's
/// `conflicts_with` already rules out both being set.
fn build_ladders(tiers: Option<&Path>, providers: &[Provider]) -> Result<Vec<Ladder>> {
    match tiers {
        Some(path) => Ok(vec![Ladder::custom(Tiers::load_file(path)?)]),
        None => providers.iter().map(|&p| Ladder::builtin(p)).collect(),
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

/// Resolves the `<ISSUE>` arguments (or `--all-open`) to issue references,
/// fetches them, in order, without duplicates, and resolves each one's open
/// citations (#1812). The current repository is looked up through `gh` only
/// when an argument needs it. **Blocking.**
fn fetch_docs(
    bin: &Path,
    cwd: &Path,
    issues: &[String],
    all_open: bool,
    max_input_chars: usize,
) -> Result<(Vec<IssueDoc>, OpenDependencies)> {
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
    let docs = fetch_issues(bin, &refs)?;
    let dependencies = find_open_dependencies(bin, &docs, max_input_chars)?;
    Ok((docs, dependencies))
}

/// Finds each doc's citations in the text `route` sends to Jev (#1812), then
/// batch-resolves every citation across every doc in one `gh` call, keeping
/// only the ones still open. An unresolved citation (typo, deleted issue) is
/// silently dropped, matching `verify-decision`'s tolerance for the same
/// case. **Blocking.**
fn find_open_dependencies(
    bin: &Path,
    docs: &[IssueDoc],
    max_input_chars: usize,
) -> Result<OpenDependencies> {
    let mut per_issue: Vec<((String, u64), Vec<Citation>)> = Vec::new();
    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for doc in docs {
        let judged = ItemRef {
            provider: doc.provider,
            project: doc.project.clone(),
            kind: doc.kind,
            number: doc.number,
        };
        let (state, _truncated) = build_route_state(doc, max_input_chars);
        let citations = find_citations(&state, &doc.project, &judged);
        for citation in &citations {
            let key = (citation.item_ref.project.clone(), citation.item_ref.number);
            if seen.insert(key) {
                refs.push(citation.item_ref.clone());
            }
        }
        per_issue.push(((doc.project.clone(), doc.number), citations));
    }
    if refs.is_empty() {
        return Ok(OpenDependencies::new());
    }

    let mut states: BTreeMap<(String, u64), ItemState> = BTreeMap::new();
    for (item_ref, fetched) in refs.iter().zip(fetch_items(bin, &refs)?) {
        if let Some(fetched) = fetched {
            states.insert((item_ref.project.clone(), item_ref.number), fetched.state);
        }
    }

    let mut dependencies = OpenDependencies::new();
    for (key, citations) in per_issue {
        let open: Vec<_> = citations
            .into_iter()
            .filter(|c| {
                states.get(&(c.item_ref.project.clone(), c.item_ref.number))
                    == Some(&ItemState::Open)
            })
            .collect();
        if !open.is_empty() {
            dependencies.insert(key, open);
        }
    }
    Ok(dependencies)
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
        assert_eq!(cmd.output, RouteFormat::Json);
        assert_eq!(cmd.providers, [Provider::Anthropic]);
        assert!(cmd.tiers.is_none());
    }

    #[test]
    fn route_parses_text_output() {
        let cmd = parse(&["#1", "-o", "text"]).unwrap();
        assert_eq!(cmd.output, RouteFormat::Text);
    }

    #[test]
    fn route_parses_providers_in_order() {
        let cmd = parse(&["#1", "--providers", "openai,gemini"]).unwrap();
        assert_eq!(cmd.providers, [Provider::OpenAi, Provider::Gemini]);
        let cmd = parse(&["#1", "--providers", "gemini", "--providers", "anthropic"]).unwrap();
        assert_eq!(cmd.providers, [Provider::Gemini, Provider::Anthropic]);
    }

    #[test]
    fn route_rejects_an_unknown_or_empty_provider() {
        let Err(err) = parse(&["#1", "--providers", "nope"]) else {
            // omni-dev: coverage ignore-line reason="guards this test's assumption; the parse above always fails on an unknown provider"
            panic!("an unknown provider parsed");
        };
        let text = err.to_string();
        assert!(text.contains("anthropic"), "{text}");
        assert!(text.contains("openai"), "{text}");
        assert!(text.contains("gemini"), "{text}");
        assert!(parse(&["#1", "--providers", ""]).is_err());
        assert!(parse(&["#1", "--providers"]).is_err());
    }

    /// A defaulted `--providers` does not conflict, so `--tiers` alone is
    /// fine; naming both is an error rather than one silently winning.
    #[test]
    fn route_tiers_conflicts_with_an_explicit_providers() {
        assert!(parse(&["#1", "--tiers", "t.yaml"]).is_ok());
        let Err(err) = parse(&["#1", "--tiers", "t.yaml", "--providers", "openai"]) else {
            // omni-dev: coverage ignore-line reason="guards this test's assumption; clap's conflicts_with always rejects --tiers with --providers"
            panic!("--tiers with --providers parsed");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn build_ladders_from_providers_preserves_order() {
        let ladders = build_ladders(None, &[Provider::Gemini, Provider::Anthropic]).unwrap();
        let names: Vec<&str> = ladders.iter().map(|l| l.provider.as_str()).collect();
        assert_eq!(names, ["gemini", "anthropic"]);
    }

    #[test]
    fn build_ladders_from_tiers_file_ignores_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiers.yaml");
        std::fs::write(
            &path,
            "tiers:\n  - {name: small, description: S}\n  - {name: big, description: B}\n",
        )
        .unwrap();
        let ladders = build_ladders(Some(&path), &[Provider::OpenAi]).unwrap();
        assert_eq!(ladders.len(), 1);
        assert_eq!(ladders[0].provider, crate::jev::route::CUSTOM_PROVIDER);
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
        use crate::jev::route::{
            IssueRoute, ProviderRoute, RouteOutcome, StageAnswer, StageAnswers,
        };
        let answer = || StageAnswer {
            choice: "sonnet".to_string(),
            confidence: 0.9,
            probabilities: BTreeMap::new(),
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
                providers: BTreeMap::from([(
                    "anthropic".to_string(),
                    ProviderRoute {
                        stages: StageAnswers {
                            design: answer(),
                            implement: answer(),
                            review: answer(),
                        },
                        class: "sonnet".to_string(),
                        close_calls: vec![],
                    },
                )]),
                depends_on: vec![],
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

    #[test]
    fn render_output_dispatches_on_format() {
        use crate::jev::protocol::Usage;
        use crate::jev::route::{
            IssueRoute, ProviderRoute, RouteOutcome, StageAnswer, StageAnswers,
        };
        let answer = || StageAnswer {
            choice: "sonnet".to_string(),
            confidence: 0.9,
            probabilities: BTreeMap::new(),
        };
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                item_ref: "o/r#1".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                outcome: RouteOutcome::Routed {
                    providers: BTreeMap::from([(
                        "anthropic".to_string(),
                        ProviderRoute {
                            stages: StageAnswers {
                                design: answer(),
                                implement: answer(),
                                review: answer(),
                            },
                            class: "sonnet".to_string(),
                            close_calls: vec![],
                        },
                    )]),
                    depends_on: vec![],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };

        let json = render_output(&report, RouteFormat::Json, DEFAULT_MAX_INPUT_CHARS).unwrap();
        assert!(json.contains("\"model\": \"jev-1.13.0\""), "{json}");

        let yaml = render_output(&report, RouteFormat::Yaml, DEFAULT_MAX_INPUT_CHARS).unwrap();
        assert!(yaml.contains("model: jev-1.13.0"), "{yaml}");

        let text = render_output(&report, RouteFormat::Text, DEFAULT_MAX_INPUT_CHARS).unwrap();
        assert!(text.starts_with("o/r#1 — t\n"), "{text}");
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
        let (docs, deps) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                dir.path(),
                &["rust-works/omni-dev#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
            )
        })
        .unwrap();
        assert_eq!(docs.len(), 1);
        assert!(deps.is_empty());
        assert_eq!(calls(dir.path()), ["api"]);
    }

    #[test]
    fn fetch_docs_resolves_bare_numbers_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let args = ["#1", "1", "rust-works/omni-dev#2"].map(str::to_string);
        let (docs, _deps) = retry_on_etxtbsy(|| {
            fetch_docs(&bin, dir.path(), &args, false, DEFAULT_MAX_INPUT_CHARS)
        })
        .unwrap();
        let numbers: Vec<u64> = docs.iter().map(|d| d.number).collect();
        assert_eq!(numbers, [1, 2]);
        assert_eq!(calls(dir.path()), ["repo", "api"]);
    }

    #[test]
    fn fetch_docs_all_open_lists_the_current_repository() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let (docs, _deps) =
            retry_on_etxtbsy(|| fetch_docs(&bin, dir.path(), &[], true, DEFAULT_MAX_INPUT_CHARS))
                .unwrap();
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
            DEFAULT_MAX_INPUT_CHARS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("-C/--repo"), "{err}");
    }

    // ── fetch_docs dependencies (#1812) ────────────────────────────────

    /// A fake `gh` whose sole issue (`#1`) cites a second, distinct issue
    /// (`#2`) — open or closed depending on `cited_state`.
    fn fake_gh_with_citation(
        dir: &Path,
        cited_state: &str,
    ) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let citing = serde_json::json!({
            "__typename": "Issue",
            "title": "t", "body": "see #2", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let cited = serde_json::json!({
            "__typename": "Issue",
            "title": "t2", "body": "b2", "state": cited_state, "url": "u2"
        });
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 repo) echo rust-works/omni-dev ;;\n\
                 *) case \"$4\" in\n\
                    *'number:2'*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {cited}}}}}}}\nJSON\n;;\n\
                    *) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {citing}}}}}}}\nJSON\n;;\n\
                    esac ;;\n\
                 esac\n",
            ),
        );
        (path, guard)
    }

    #[test]
    fn fetch_docs_reports_an_open_citation_as_a_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_citation(dir.path(), "OPEN");
        let (docs, deps) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
            )
        })
        .unwrap();
        let open = deps
            .get(&(docs[0].project.clone(), docs[0].number))
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].raw, "#2");
    }

    #[test]
    fn fetch_docs_drops_a_closed_citation() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_citation(dir.path(), "CLOSED");
        let (_docs, deps) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
            )
        })
        .unwrap();
        assert!(deps.is_empty(), "{deps:?}");
    }
}
