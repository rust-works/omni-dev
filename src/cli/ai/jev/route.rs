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
    build_route_state, render_route_text, run_route_with_reference_fetch_failures, Ladder,
    OpenDependencies, Provider, ReferenceFetchFailure, ReferenceFetchFailures, RouteOptions,
    RouteReport, Tiers, DEFAULT_CLOSE_CALL, DEFAULT_MAX_INPUT_CHARS,
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
--close-call is listed under close_calls.\n\nThe classes come from a named model ladder: \
built-in ladders are anthropic (sonnet/opus/fable, the default), openai (terra/sol/astra) \
and gemini (flash/pro/deep-think). --ladders NAMES routes against several ladders at once, \
still in one Jev call per issue, and the output nests stages, class and close_calls under \
each ladder's name. --ladder-definition NAME=FILE registers a custom ladder under NAME, its \
tiers loaded from a YAML file, so it can be routed alongside built-in ladders in the same \
--ladders list; a built-in name cannot be redefined, and a definition never listed in \
--ladders is an error.\n\nEach open issue or pull request the text \
cites is reported under depends_on, with a could_be_cheaper.design probability: how likely it \
is that resolving that dependency would leave less design work remaining than the text \
implies. One extra Jev question is asked per open citation; a closed citation is settled and \
is not reported. A cited issue or pull request that cannot be fetched is listed under \
reference_fetch_failures instead.\n\nClosed issues are refused unless --allow-closed is given: their comments \
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

    /// Ladder names to route against (comma-separated, repeatable): a built-in provider
    /// name or one registered via --ladder-definition.
    #[arg(
        long,
        value_name = "NAMES",
        value_delimiter = ',',
        default_value = "anthropic"
    )]
    pub ladders: Vec<String>,

    /// Registers a custom ladder as NAME=FILE, its tiers loaded from FILE (same YAML shape
    /// as a tiers file: `tiers: [{name, description}, ...]`). Repeatable.
    #[arg(long, value_name = "NAME=FILE", value_parser = parse_ladder_definition)]
    pub ladder_definition: Vec<(String, PathBuf)>,

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
        let ladders = build_ladders(&self.ladders, &self.ladder_definition)?;
        let client = JevClient::from_config(&config)?;

        let bin = crate::pr_status::resolve_gh_binary();
        let cwd = self
            .repo
            .path()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let (issues, all_open, max_input_chars) =
            (self.issues, self.all_open, self.max_input_chars);
        let (docs, dependencies, reference_fetch_failures) =
            tokio::task::spawn_blocking(move || {
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
        let report = run_route_with_reference_fetch_failures(
            &client,
            &docs,
            &ladders,
            &opts,
            &dependencies,
            &reference_fetch_failures,
        )
        .await?;
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

/// Parses a `NAME=FILE` `--ladder-definition` value.
///
/// Splits on the **first** `=` only, so a later `=` stays inside the path; an
/// empty `NAME` is rejected. Wired as a clap `value_parser`.
fn parse_ladder_definition(s: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = s
        .split_once('=')
        .ok_or_else(|| format!("`{s}` is not in NAME=FILE form"))?;
    if name.is_empty() {
        return Err(format!("`{s}` has an empty ladder name"));
    }
    Ok((name.to_string(), PathBuf::from(path)))
}

/// Validates a `--ladder-definition` name: non-empty, `[a-z0-9_-]+`, and not
/// a built-in provider's name — built-in names are reserved rather than
/// silently overridable.
fn validate_ladder_name(name: &str) -> Result<()> {
    let valid_charset = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid_charset {
        bail!("ladder name {name:?} must be non-empty and match [a-z0-9_-]+");
    }
    if Provider::ALL.iter().any(|p| p.name() == name) {
        bail!("ladder name {name:?} is a built-in provider name and cannot be redefined");
    }
    Ok(())
}

/// Resolves `--ladders`/`--ladder-definition` to the ladders to route
/// against, in `--ladders`' order (#1826). Each `--ladders` entry is either a
/// built-in provider's name or one registered via `--ladder-definition`; a
/// `--ladder-definition` never referenced by `--ladders` is an error, since
/// that is more likely a typo'd `--ladders` entry than an intentionally
/// unused definition.
fn build_ladders(ladders: &[String], definitions: &[(String, PathBuf)]) -> Result<Vec<Ladder>> {
    let mut custom: BTreeMap<&str, &Path> = BTreeMap::new();
    for (name, path) in definitions {
        validate_ladder_name(name)?;
        if custom.insert(name.as_str(), path.as_path()).is_some() {
            bail!("ladder definition {name:?} is given more than once");
        }
    }

    let mut used = HashSet::new();
    let result = ladders
        .iter()
        .map(|name| {
            used.insert(name.as_str());
            if let Some(provider) = Provider::ALL
                .into_iter()
                .find(|p| p.name() == name.as_str())
            {
                Ladder::builtin(provider)
            } else if let Some(&path) = custom.get(name.as_str()) {
                Ok(Ladder::named(name.clone(), Tiers::load_file(path)?))
            } else {
                let mut known: Vec<&str> =
                    Provider::ALL.iter().map(|p| p.name()).collect::<Vec<_>>();
                known.extend(custom.keys().copied());
                bail!(
                    "unknown ladder {name:?}; known ladders are {}",
                    known.join(", ")
                )
            }
        })
        .collect::<Result<Vec<_>>>()?;

    if let Some(unused) = custom.keys().find(|name| !used.contains(*name)) {
        bail!("ladder definition {unused:?} was never listed in --ladders");
    }
    Ok(result)
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
) -> Result<(Vec<IssueDoc>, OpenDependencies, ReferenceFetchFailures)> {
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
    let (dependencies, reference_fetch_failures) =
        find_open_dependencies(bin, &docs, max_input_chars)?;
    Ok((docs, dependencies, reference_fetch_failures))
}

/// Finds each doc's citations in the text `route` sends to Jev (#1812), then
/// batch-resolves every citation across every doc in one `gh` call, keeping
/// only the ones still open. An unresolved citation (typo, deleted issue) is
/// retained as a reference-fetch failure for output. **Blocking.**
fn find_open_dependencies(
    bin: &Path,
    docs: &[IssueDoc],
    max_input_chars: usize,
) -> Result<(OpenDependencies, ReferenceFetchFailures)> {
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
        return Ok((OpenDependencies::new(), ReferenceFetchFailures::new()));
    }

    let mut states: BTreeMap<(String, u64), Option<ItemState>> = BTreeMap::new();
    for (item_ref, fetched) in refs.iter().zip(fetch_items(bin, &refs)?) {
        states.insert(
            (item_ref.project.clone(), item_ref.number),
            fetched.map(|doc| doc.state),
        );
    }

    let mut dependencies = OpenDependencies::new();
    let mut reference_fetch_failures = ReferenceFetchFailures::new();
    for (key, citations) in per_issue {
        let open: Vec<_> = citations
            .iter()
            .filter(|c| {
                states.get(&(c.item_ref.project.clone(), c.item_ref.number))
                    == Some(&Some(ItemState::Open))
            })
            .cloned()
            .collect();
        if !open.is_empty() {
            dependencies.insert(key.clone(), open);
        }
        let failures = citations
            .into_iter()
            .filter(|c| states.get(&(c.item_ref.project.clone(), c.item_ref.number)) == Some(&None))
            .map(|c| ReferenceFetchFailure {
                item_ref: c.raw,
                error: "not found".to_string(),
            })
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            reference_fetch_failures.insert(key, failures);
        }
    }
    Ok((dependencies, reference_fetch_failures))
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
        assert_eq!(cmd.ladders, ["anthropic"]);
        assert!(cmd.ladder_definition.is_empty());
    }

    #[test]
    fn route_parses_text_output() {
        let cmd = parse(&["#1", "-o", "text"]).unwrap();
        assert_eq!(cmd.output, RouteFormat::Text);
    }

    #[test]
    fn route_parses_ladders_in_order() {
        let cmd = parse(&["#1", "--ladders", "openai,gemini"]).unwrap();
        assert_eq!(cmd.ladders, ["openai", "gemini"]);
        let cmd = parse(&["#1", "--ladders", "gemini", "--ladders", "anthropic"]).unwrap();
        assert_eq!(cmd.ladders, ["gemini", "anthropic"]);
    }

    #[test]
    fn route_accepts_a_ladder_definition() {
        let cmd = parse(&[
            "#1",
            "--ladders",
            "anthropic,mine",
            "--ladder-definition",
            "mine=my-tiers.yaml",
        ])
        .unwrap();
        assert_eq!(
            cmd.ladder_definition,
            [("mine".to_string(), PathBuf::from("my-tiers.yaml"))]
        );
    }

    #[test]
    fn clap_rejects_a_malformed_ladder_definition() {
        let Err(err) = parse(&["#1", "--ladder-definition", "noequals"]) else {
            // omni-dev: coverage ignore-line reason="guards this test's assumption; the parse above always fails on a malformed --ladder-definition"
            panic!("a malformed --ladder-definition parsed");
        };
        assert!(err.to_string().contains("NAME=FILE"), "{err}");
    }

    #[test]
    fn clap_still_requires_a_value_for_ladders() {
        assert!(parse(&["#1", "--ladders"]).is_err());
    }

    // ── build_ladders ────────────────────────────────────────────────

    #[test]
    fn build_ladders_preserves_ladders_order() {
        let ladders = build_ladders(&["gemini".to_string(), "anthropic".to_string()], &[]).unwrap();
        let names: Vec<&str> = ladders.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["gemini", "anthropic"]);
    }

    fn write_tiers_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            "tiers:\n  - {name: small, description: S}\n  - {name: big, description: B}\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn build_ladders_resolves_a_custom_ladder_definition() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tiers_file(dir.path(), "tiers.yaml");
        let ladders = build_ladders(&["mine".to_string()], &[("mine".to_string(), path)]).unwrap();
        assert_eq!(ladders.len(), 1);
        assert_eq!(ladders[0].name, "mine");
    }

    #[test]
    fn build_ladders_combines_a_builtin_and_a_custom_ladder_in_one_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tiers_file(dir.path(), "tiers.yaml");
        let ladders = build_ladders(
            &["anthropic".to_string(), "mine".to_string()],
            &[("mine".to_string(), path)],
        )
        .unwrap();
        let names: Vec<&str> = ladders.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["anthropic", "mine"]);
    }

    #[test]
    fn build_ladders_rejects_an_unknown_ladder_name() {
        let err = build_ladders(&["nope".to_string()], &[]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("anthropic"), "{text}");
        assert!(text.contains("openai"), "{text}");
        assert!(text.contains("gemini"), "{text}");
    }

    #[test]
    fn build_ladders_rejects_a_duplicate_ladder_definition_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tiers_file(dir.path(), "tiers.yaml");
        let err = build_ladders(
            &["mine".to_string()],
            &[
                ("mine".to_string(), path.clone()),
                ("mine".to_string(), path),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("given more than once"), "{err}");
    }

    #[test]
    fn build_ladders_rejects_an_unused_ladder_definition() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tiers_file(dir.path(), "tiers.yaml");
        let err =
            build_ladders(&["anthropic".to_string()], &[("mine".to_string(), path)]).unwrap_err();
        assert!(
            err.to_string().contains("\"mine\" was never listed"),
            "{err}"
        );
    }

    #[test]
    fn build_ladders_rejects_a_ladder_definition_that_shadows_a_builtin_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tiers_file(dir.path(), "tiers.yaml");
        let err = build_ladders(
            &["anthropic".to_string()],
            &[("anthropic".to_string(), path)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot be redefined"), "{err}");
    }

    #[test]
    fn build_ladders_rejects_an_invalid_ladder_name_charset() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tiers_file(dir.path(), "tiers.yaml");
        let err = build_ladders(
            &["My.Ladder".to_string()],
            &[("My.Ladder".to_string(), path)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("[a-z0-9_-]+"), "{err}");
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
                reference_fetch_failures: vec![],
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
            IssueRoute, ProviderRoute, ReferenceFetchFailure, RouteOutcome, StageAnswer,
            StageAnswers,
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
                    reference_fetch_failures: vec![ReferenceFetchFailure {
                        item_ref: "#404".to_string(),
                        error: "not found".to_string(),
                    }],
                },
                truncated: false,
            }],
            usage: Usage::default(),
        };

        let json = render_output(&report, RouteFormat::Json, DEFAULT_MAX_INPUT_CHARS).unwrap();
        assert!(json.contains("\"model\": \"jev-1.13.0\""), "{json}");
        assert!(json.contains("\"reference_fetch_failures\""), "{json}");
        assert!(json.contains("\"ref\": \"#404\""), "{json}");

        let yaml = render_output(&report, RouteFormat::Yaml, DEFAULT_MAX_INPUT_CHARS).unwrap();
        assert!(yaml.contains("model: jev-1.13.0"), "{yaml}");
        assert!(yaml.contains("reference_fetch_failures:"), "{yaml}");
        assert!(yaml.contains("ref: '#404'"), "{yaml}");

        let text = render_output(&report, RouteFormat::Text, DEFAULT_MAX_INPUT_CHARS).unwrap();
        assert!(text.starts_with("o/r#1 — t\n"), "{text}");
        assert!(
            text.contains("reference fetch failed: #404 (not found)"),
            "{text}"
        );
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
        let (docs, deps, _failures) = retry_on_etxtbsy(|| {
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
        let (docs, _deps, _failures) = retry_on_etxtbsy(|| {
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
        let (docs, _deps, _failures) =
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

    /// A fake `gh` whose cited issue is absent. The citation request models
    /// GitHub CLI's partial GraphQL response: useful JSON on stdout and exit
    /// status 1 because the individual node was not found.
    fn fake_gh_with_missing_citation(dir: &Path) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let citing = serde_json::json!({
            "__typename": "Issue",
            "title": "t", "body": "see #2", "state": "OPEN", "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n\\
                 repo) echo rust-works/omni-dev ;;\n\\
                 *) case \"$4\" in\n\\
                    *'number:2'*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": null}}}}, \"errors\": [{{\"type\": \"NOT_FOUND\", \"path\": [\"r0\", \"i0\"], \"message\": \"Could not resolve\"}}]}}\nJSON\nexit 1 ;;\n\\
                    *) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {citing}}}}}}}\nJSON\n;;\n\\
                    esac ;;\n\\
                 esac\n",
            ),
        );
        (path, guard)
    }

    #[test]
    fn fetch_docs_reports_an_open_citation_as_a_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_citation(dir.path(), "OPEN");
        let (docs, deps, _failures) = retry_on_etxtbsy(|| {
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
        let (_docs, deps, _failures) = retry_on_etxtbsy(|| {
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

    #[test]
    fn fetch_docs_reports_a_missing_citation_as_a_reference_fetch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_missing_citation(dir.path());
        let (docs, dependencies, failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
            )
        })
        .unwrap();
        let key = (docs[0].project.clone(), docs[0].number);
        assert!(dependencies.is_empty(), "{dependencies:?}");
        assert_eq!(
            failures.get(&key),
            Some(&vec![ReferenceFetchFailure {
                item_ref: "#2".to_string(),
                error: "not found".to_string(),
            }])
        );
    }
}
