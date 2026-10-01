//! `ai jev route` — routes issues to model classes by stage.

use std::collections::{BTreeMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::github_issues::{
    fetch_issues_cached_current, fetch_items_cached_current, list_open_issue_numbers,
    needs_default_project, parse_issue_arg, resolve_current_project, CacheUsage, IssueCache,
};
use crate::jev::citations::{find_citations, Citation};
use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::route::{
    build_route_citation_text_with_drafts, is_ignored_closed, render_route_text,
    render_route_text_styled, run_route_with_reference_fetch_failures, Ladder, OpenDependencies,
    Provider, ReferenceFetchFailure, ReferenceFetchFailures, RouteOptions, RouteReport,
    TerminalStyle, Tiers, DEFAULT_CLOSE_CALL, DEFAULT_CLOSE_CALL_MARGIN, DEFAULT_MAX_INPUT_CHARS,
};
use crate::provider::{GitProvider, IssueDoc, ItemKind, ItemRef, ItemState};
use crate::utils::env::{EnvSource, SystemEnv};

use super::common::{format_output_with_cache, JevFormat};

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

/// Routes issues to model classes for design, implement and review.
#[derive(Parser)]
#[command(
    long_about = "Routes issues to model classes for their design, implement and review \
stages. Add --effort-advice for per-model effort recommendations.\n\nMakes one Jev call per issue. Jev judges the issue's title, body and human \
comments (never the issues or pull requests it references) and picks, for each stage, the \
least capable class likely to do it correctly with no rework. The issue's class is the \
higher of its design and implement choices (class_from names which one supplied it; a tie \
goes to implement), and a stage whose confidence is below --close-call or whose top-two \
model-class probability gap is below --close-call-margin is listed under close_calls. \
Both comparisons are strict; --close-call-margin 0 disables margin flagging. Effort \
advice uses only the confidence threshold.\n\nThe classes come from a named model ladder: \
built-in ladders are anthropic (sonnet/opus, the default), openai (terra/sol/astra) \
and gemini (flash/pro/deep-think). --ladders NAMES routes against several ladders at once, \
still in one Jev call per issue, and the output nests stages, class, class_from and \
close_calls under each ladder's name. --ladder-definition NAME=FILE registers a custom ladder under NAME, its \
tiers loaded from a YAML file, so it can be routed alongside built-in ladders in the same \
--ladders list; a built-in name cannot be redefined, and a definition never listed in \
--ladders is an error.\n\nWith --effort-advice, each stage also reports effort_by_model for every rung, \
using each model's supported native levels. This asks additional Jev questions and increases \
token use. Without the flag, no effort questions are asked and effort_by_model is omitted. \
Legacy custom ladders without effort metadata report unspecified when enabled. A stage with no \
remaining work reports not_needed; a model that cannot meet the reliability bar reports \
insufficient. Effort advice does not execute or configure a model or Jev's own effort. See \
docs/jev.md for the \
ladder schema and calibration limits.\n\nEach open issue or pull request the text \
cites is reported under depends_on, with a could_be_cheaper.design probability: how likely it \
is that resolving that dependency would leave less design work remaining than the text \
implies. One extra Jev question is asked per open citation; a closed citation is settled and \
is not reported. A cited issue or pull request that cannot be fetched is listed under \
reference_fetch_failures instead.\n\nClosed issues are refused unless --allow-closed is given: their comments \
often describe how the work was done, which leaks the answer. --ignore-closed instead drops \
closed issues from the batch without routing them, so one closed issue in a long list does \
not abort the run; the skipped issues are listed under ignored_closed, and a batch with no \
open issue left is still an error.\n\nAn issue whose Jev call \
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

    /// Append a local UTF-8 comment for preview without posting it. Repeatable; one issue only.
    #[arg(long, value_name = "FILE", conflicts_with = "all_open")]
    pub draft_comment: Vec<PathBuf>,

    /// Ladder names to route against (comma-separated, repeatable): a built-in provider
    /// name or one registered via --ladder-definition.
    #[arg(
        long,
        value_name = "NAMES",
        value_delimiter = ',',
        default_value = "anthropic"
    )]
    pub ladders: Vec<String>,

    /// Registers a custom ladder as NAME=FILE. Tiers contain name, description, and optional
    /// model-specific effort profiles (see docs/jev.md). Repeatable.
    #[arg(long, value_name = "NAME=FILE", value_parser = parse_ladder_definition)]
    pub ladder_definition: Vec<(String, PathBuf)>,

    /// Ask for per-model effort advice in text, JSON, or YAML output.
    #[arg(long)]
    pub effort_advice: bool,

    /// Confidence below which a model-class or effort answer is reported as a close call.
    #[arg(long, value_name = "CONFIDENCE", default_value_t = DEFAULT_CLOSE_CALL)]
    pub close_call: f64,

    /// Top-two model-class probability gap below which a stage is a close call (strictly less;
    /// 0 disables margin flagging). Effort advice uses only --close-call.
    #[arg(long, value_name = "MARGIN", default_value_t = DEFAULT_CLOSE_CALL_MARGIN,
        value_parser = parse_close_call_margin)]
    pub close_call_margin: f64,

    /// Longest issue text, in characters, sent to Jev; longer text is cut with a marker.
    #[arg(long, value_name = "CHARS", default_value_t = DEFAULT_MAX_INPUT_CHARS)]
    pub max_input_chars: usize,

    /// Routes closed issues instead of refusing them.
    #[arg(long)]
    pub allow_closed: bool,

    /// Skips closed issues instead of refusing them, listing them under `ignored_closed`.
    #[arg(long, conflicts_with = "allow_closed")]
    pub ignore_closed: bool,

    /// Re-fetches every issue from GitHub instead of reusing one fetched in the last few
    /// minutes (see OMNI_DEV_GITHUB_CACHE_TTL_SECS).
    #[arg(long)]
    pub refresh: bool,

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

fn parse_close_call_margin(value: &str) -> std::result::Result<f64, String> {
    let margin: f64 = value
        .parse()
        .map_err(|_| "margin must be a number".to_string())?;
    if !(0.0..=1.0).contains(&margin) {
        return Err("margin must be finite and between 0 and 1".to_string());
    }
    Ok(margin)
}

impl RouteCommand {
    /// Executes the route command.
    pub async fn execute(self) -> Result<()> {
        // omni-dev: coverage ignore reason="RouteCommand::execute is the process-bound wiring shell; fetch_docs, run_route_with_reference_fetch_failures, render_output_with_style and terminal_style_with provide its deterministic seams"
        let drafts = self.load_draft_comments()?;
        let env = crate::utils::settings::SettingsEnv::load();
        let mut config = JevConfig::from_env_with(&env)?;
        if let Some(model) = &self.jev_model {
            config.model = model.clone();
        }
        let ladders = build_ladders(&self.ladders, &self.ladder_definition)?;
        let client = JevClient::from_config(&config)?;
        let mut opts = self.route_options(config.model);
        opts.draft_comments = drafts.clone();

        let bin = crate::pr_status::resolve_gh_binary();
        let cwd = self
            .repo
            .path()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let (issues, all_open, max_input_chars, ignore_closed) = (
            self.issues,
            self.all_open,
            self.max_input_chars,
            self.ignore_closed,
        );
        let cache = crate::github_issues::open_cache_blocking(
            env,
            dirs::cache_dir(),
            bin.clone(),
            self.refresh,
        )
        .await?;
        let fetch_cache = std::sync::Arc::clone(&cache);
        let fetched = tokio::task::spawn_blocking(move || {
            fetch_docs(
                &bin,
                &fetch_cache,
                &cwd,
                &issues,
                all_open,
                max_input_chars,
                ignore_closed,
                &drafts,
            )
        })
        .await
        .context("Issue fetch task panicked")?;
        // Before `?`, so a failure on cached input still names --refresh.
        if let Some(note) = cache.reuse_note() {
            eprintln!("{note}");
        }
        let (docs, dependencies, reference_fetch_failures) = fetched?;

        let mut report = run_route_with_reference_fetch_failures(
            &client,
            &docs,
            &ladders,
            &opts,
            &dependencies,
            &reference_fetch_failures,
        )
        .await?;
        for issue in &mut report.issues {
            issue.draft_comments.clone_from(&self.draft_comment);
        }
        print!(
            "{}",
            render_output_with_style(
                &report,
                self.output,
                self.max_input_chars,
                &ladders,
                terminal_style(),
                cache.usage(),
            )?
        );
        failure_summary(&report).map_or(Ok(()), |msg| bail!(msg))
        // omni-dev: coverage end
    }

    fn load_draft_comments(&self) -> Result<Vec<String>> {
        if !self.draft_comment.is_empty() && (self.all_open || self.issues.len() != 1) {
            bail!("--draft-comment applies to exactly one ISSUE; cannot preview --all-open or multiple issues");
        }
        self.draft_comment
            .iter()
            .map(|path| {
                let body = std::fs::read_to_string(path)
                    .with_context(|| format!("Failed to read draft comment {}", path.display()))?;
                if body.trim().is_empty() {
                    bail!("Draft comment {} is empty", path.display());
                }
                Ok(body)
            })
            .collect()
    }

    fn route_options(&self, model: String) -> RouteOptions {
        RouteOptions {
            draft_comments: vec![],
            model,
            close_call: self.close_call,
            close_call_margin: self.close_call_margin,
            effort_advice: self.effort_advice,
            max_input_chars: self.max_input_chars,
            allow_closed: self.allow_closed,
            ignore_closed: self.ignore_closed,
        }
    }
}

/// Renders a routed report in `output`'s format. Split out of
/// [`RouteCommand::execute`] so the format dispatch is unit-testable without
/// a Jev client or a `gh` binary — `execute` itself stays an untested thin
/// shell, per the project's `run_*` convention (STYLE-0025).
///
/// `cache_use` becomes `github_cache` in JSON and YAML; the text format is for
/// people, who get the same fact from the stderr note.
fn render_output(
    report: &RouteReport,
    output: RouteFormat,
    max_input_chars: usize,
    cache_use: Option<CacheUsage>,
) -> Result<String> {
    Ok(match output {
        RouteFormat::Json => format_output_with_cache(report, cache_use, JevFormat::Json)?,
        RouteFormat::Yaml => format_output_with_cache(report, cache_use, JevFormat::Yaml)?,
        RouteFormat::Text => render_route_text(report, max_input_chars),
    })
}

fn render_output_with_style(
    report: &RouteReport,
    output: RouteFormat,
    max_input_chars: usize,
    ladders: &[Ladder],
    style: TerminalStyle,
    cache_use: Option<CacheUsage>,
) -> Result<String> {
    if output == RouteFormat::Text {
        Ok(render_route_text_styled(
            report,
            max_input_chars,
            ladders,
            style,
        ))
    } else {
        render_output(report, output, max_input_chars, cache_use)
    }
}

// omni-dev: coverage ignore reason="the SystemEnv/stdout probe is process-bound; terminal_style_with is exhaustively covered through its injected environment and TTY seam"
/// Whether stdout may carry colour and OSC 8 links.
fn terminal_style() -> TerminalStyle {
    terminal_style_with(&SystemEnv, std::io::stdout().is_terminal())
}
// omni-dev: coverage end

/// The env-parsing seam behind [`terminal_style`] (STYLE-0028): given these
/// variables and whether stdout is a terminal, what may be emitted?
///
/// OSC 8 has no universal capability query, so links are opt-in for terminals
/// known to support them. Colour and links are independent: `NO_COLOR` only
/// disables SGR.
fn terminal_style_with(env: &impl EnvSource, tty: bool) -> TerminalStyle {
    let supports_links = matches!(
        env.var("TERM_PROGRAM").as_deref(),
        Some("iTerm.app" | "WezTerm" | "vscode" | "Hyper" | "ghostty")
    ) || [
        "WT_SESSION",
        "KITTY_WINDOW_ID",
        "KONSOLE_VERSION",
        // Only set from Alacritty 0.12, which is past 0.11's OSC 8 support.
        "ALACRITTY_WINDOW_ID",
    ]
    .iter()
    .any(|key| env.var(key).is_some())
        || env
            .var("VTE_VERSION")
            .and_then(|v| v.parse::<u32>().ok())
            .is_some_and(|v| v >= 5000);
    TerminalStyle::new(
        tty,
        &env.var("TERM").unwrap_or_default(),
        // no-color.org: the variable suppresses colour only when non-empty,
        // so `NO_COLOR=` neutralises an exported value for one invocation.
        env.var("NO_COLOR").is_some_and(|v| !v.is_empty()),
        supports_links,
    )
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
/// when an argument needs it. With `ignore_closed`, a closed issue's
/// citations are not resolved, since the engine will skip it (#2000).
/// **Blocking.**
#[allow(clippy::too_many_arguments)]
fn fetch_docs(
    bin: &Path,
    cache: &IssueCache,
    cwd: &Path,
    issues: &[String],
    all_open: bool,
    max_input_chars: usize,
    ignore_closed: bool,
    drafts: &[String],
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
    // The listing already proves every `--all-open` issue is open, so a cached
    // copy that says otherwise (a reopened issue) is refetched (#2041).
    let listed = all_open.then_some(ItemState::Open);
    let docs = fetch_issues_cached_current(bin, cache, &refs, listed)?;
    let routable: Vec<&IssueDoc> = docs
        .iter()
        .filter(|d| !is_ignored_closed(d, ignore_closed))
        .collect();
    let (dependencies, reference_fetch_failures) =
        find_open_dependencies(bin, cache, &routable, max_input_chars, drafts)?;
    Ok((docs, dependencies, reference_fetch_failures))
}

/// Finds each doc's citations in the text `route` sends to Jev (#1812), then
/// batch-resolves every citation across every doc in one `gh` call, keeping
/// only the ones still open. An unresolved citation (typo, deleted issue) is
/// retained as a reference-fetch failure for output. **Blocking.**
fn find_open_dependencies(
    bin: &Path,
    cache: &IssueCache,
    docs: &[&IssueDoc],
    max_input_chars: usize,
    drafts: &[String],
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
        let text = build_route_citation_text_with_drafts(doc, max_input_chars, drafts);
        let citations = find_citations(&text, &doc.project, &judged);
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

    let mut resolved: BTreeMap<(String, u64), Option<(ItemState, String)>> = BTreeMap::new();
    for (item_ref, fetched) in refs
        .iter()
        .zip(fetch_items_cached_current(bin, cache, &refs, None)?)
    {
        resolved.insert(
            (item_ref.project.clone(), item_ref.number),
            fetched.map(|doc| (doc.state, doc.url)),
        );
    }

    let mut dependencies = OpenDependencies::new();
    let mut reference_fetch_failures = ReferenceFetchFailures::new();
    for (key, citations) in per_issue {
        let open: Vec<_> = citations
            .iter()
            .filter_map(|c| {
                let (_, url) = resolved
                    .get(&(c.item_ref.project.clone(), c.item_ref.number))
                    .and_then(Option::as_ref)
                    .filter(|(state, _)| *state == ItemState::Open)?;
                let mut c = c.clone();
                c.url = Some(url.clone());
                Some(c)
            })
            .collect();
        if !open.is_empty() {
            dependencies.insert(key.clone(), open);
        }
        let failures = citations
            .into_iter()
            .filter(|c| {
                resolved.get(&(c.item_ref.project.clone(), c.item_ref.number)) == Some(&None)
            })
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
    fn draft_comments_load_in_order_and_preserve_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.md");
        let second = dir.path().join("second.md");
        std::fs::write(&first, "  décision\n\n  indented\n").unwrap();
        std::fs::write(&second, "second").unwrap();
        let cmd = parse(&[
            "#1",
            "--draft-comment",
            first.to_str().unwrap(),
            "--draft-comment",
            second.to_str().unwrap(),
            "--draft-comment",
            first.to_str().unwrap(),
        ])
        .unwrap();
        assert_eq!(
            cmd.draft_comment,
            [first, second, cmd.draft_comment[0].clone()]
        );
        assert_eq!(
            cmd.load_draft_comments().unwrap(),
            [
                "  décision\n\n  indented\n",
                "second",
                "  décision\n\n  indented\n"
            ]
        );
        assert!(parse(&["#1"])
            .unwrap()
            .load_draft_comments()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn draft_comments_reject_batches_before_reading_files() {
        assert!(parse(&["--all-open", "--draft-comment", "missing.md"]).is_err());
        for issues in [["#1", "#2"], ["#1", "1"]] {
            let cmd = parse(&[issues[0], issues[1], "--draft-comment", "missing.md"]).unwrap();
            assert!(cmd
                .load_draft_comments()
                .unwrap_err()
                .to_string()
                .contains("exactly one ISSUE"));
        }
    }

    #[test]
    fn draft_comment_file_errors_name_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("draft.md");
        let error = || {
            parse(&["#1", "--draft-comment", path.to_str().unwrap()])
                .unwrap()
                .load_draft_comments()
                .unwrap_err()
                .to_string()
        };
        assert!(error().contains(path.to_str().unwrap()));
        for bytes in [b"".as_slice(), b" \n\t", &[0xff]] {
            std::fs::write(&path, bytes).unwrap();
            assert!(error().contains(path.to_str().unwrap()));
        }
    }

    #[test]
    fn draft_only_citations_are_resolved_using_retained_masked_input() {
        for state in ["OPEN", "CLOSED"] {
            let dir = tempfile::tempdir().unwrap();
            let (bin, _shim) = fake_gh_with_states(dir.path(), "OPEN", state);
            let script = std::fs::read_to_string(&bin)
                .unwrap()
                .replace("see #2", "No references.");
            std::fs::write(&bin, script).unwrap();
            let drafts = ["`#99` and #2 and #2 and #1".into()];
            let (docs, fetched_deps, fetched_failures) = retry_on_etxtbsy(|| {
                fetch_docs(
                    &bin,
                    &IssueCache::disabled(),
                    dir.path(),
                    &["#1".into()],
                    false,
                    DEFAULT_MAX_INPUT_CHARS,
                    false,
                    &drafts,
                )
            })
            .unwrap();
            assert_eq!(docs[0].body, "No references.");
            assert!(docs[0].comments.is_empty());
            let (deps, failures) = retry_on_etxtbsy(|| {
                find_open_dependencies(
                    &bin,
                    &IssueCache::disabled(),
                    &[&docs[0]],
                    DEFAULT_MAX_INPUT_CHARS,
                    &drafts,
                )
            })
            .unwrap();
            assert_eq!(deps, fetched_deps);
            assert_eq!(failures, fetched_failures);
            assert!(failures.is_empty());
            if state == "OPEN" {
                assert_eq!(deps[&(docs[0].project.clone(), 1)].len(), 1);
                assert_eq!(deps[&(docs[0].project.clone(), 1)][0].raw, "#2");
            } else {
                assert!(deps.is_empty());
            }
            let (deps, failures) =
                find_open_dependencies(&bin, &IssueCache::disabled(), &[&docs[0]], 1, &drafts)
                    .unwrap();
            assert!(deps.is_empty());
            assert!(failures.is_empty());
        }
    }

    #[test]
    fn unresolved_draft_citation_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_missing_citation(dir.path());
        let (mut docs, _, _) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &IssueCache::disabled(),
                dir.path(),
                &["#1".into()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        docs[0].body.clear();
        let (deps, failures) = retry_on_etxtbsy(|| {
            find_open_dependencies(
                &bin,
                &IssueCache::disabled(),
                &[&docs[0]],
                DEFAULT_MAX_INPUT_CHARS,
                &["See #2".into()],
            )
        })
        .unwrap();
        assert!(deps.is_empty());
        assert_eq!(failures[&(docs[0].project.clone(), 1)][0].item_ref, "#2");
    }

    #[test]
    fn route_parses_issues_and_defaults() {
        let cmd = parse(&["#12", "owner/repo#3"]).unwrap();
        assert_eq!(cmd.issues, ["#12", "owner/repo#3"]);
        assert!((cmd.close_call - DEFAULT_CLOSE_CALL).abs() < f64::EPSILON);
        assert_eq!(cmd.max_input_chars, DEFAULT_MAX_INPUT_CHARS);
        assert!(!cmd.allow_closed);
        assert!(!cmd.ignore_closed);
        assert!(!cmd.refresh);
        assert!(!cmd.effort_advice);
        assert_eq!(cmd.output, RouteFormat::Json);
        assert_eq!(cmd.ladders, ["anthropic"]);
        assert!(cmd.ladder_definition.is_empty());
    }

    #[test]
    fn route_parses_refresh() {
        assert!(parse(&["#1", "--refresh"]).unwrap().refresh);
    }

    #[test]
    fn route_parses_output_format_and_effort_advice_independently() {
        let cmd = parse(&["#1", "-o", "text"]).unwrap();
        assert_eq!(cmd.output, RouteFormat::Text);
        assert!(!cmd.effort_advice);
        for (format, expected) in [
            ("text", RouteFormat::Text),
            ("json", RouteFormat::Json),
            ("yaml", RouteFormat::Yaml),
        ] {
            let cmd = parse(&["#1", "-o", format, "--effort-advice"]).unwrap();
            assert_eq!(cmd.output, expected);
            assert!(cmd.effort_advice);
        }
    }

    #[test]
    fn route_options_forward_parsed_flags_in_both_effort_modes() {
        let default = parse(&["#1"])
            .unwrap()
            .route_options("configured-model".into());
        assert_eq!(default.model, "configured-model");
        assert!(!default.effort_advice);
        assert!(!default.allow_closed);
        assert!(!default.ignore_closed);
        assert_eq!(default.max_input_chars, DEFAULT_MAX_INPUT_CHARS);
        assert!((default.close_call - DEFAULT_CLOSE_CALL).abs() < f64::EPSILON);
        assert_eq!(default.close_call_margin, DEFAULT_CLOSE_CALL_MARGIN);

        let enabled = parse(&[
            "#1",
            "--effort-advice",
            "--allow-closed",
            "--max-input-chars",
            "1234",
            "--close-call",
            "0.42",
            "--close-call-margin",
            "0.15",
        ])
        .unwrap()
        .route_options("override-model".into());
        assert_eq!(enabled.model, "override-model");
        assert!(enabled.effort_advice);
        assert!(enabled.allow_closed);
        assert_eq!(enabled.max_input_chars, 1234);
        assert!((enabled.close_call - 0.42).abs() < f64::EPSILON);
        assert_eq!(enabled.close_call_margin, 0.15);
    }

    #[test]
    fn route_validates_and_forwards_close_call_margin() {
        for margin in ["0", "0.15", "1"] {
            for effort in [false, true] {
                let mut args = vec!["#1", "--close-call-margin", margin];
                if effort {
                    args.push("--effort-advice");
                }
                let options = parse(&args).unwrap().route_options("model".into());
                assert_eq!(options.close_call_margin, margin.parse::<f64>().unwrap());
            }
        }
        for margin in ["-0.1", "1.1", "NaN", "inf", "-inf", "invalid"] {
            let arg = format!("--close-call-margin={margin}");
            assert!(parse(&["#1", &arg]).is_err(), "{margin}");
        }
    }

    #[test]
    fn route_forwards_ignore_closed() {
        let opts = parse(&["#1", "#2", "--ignore-closed"])
            .unwrap()
            .route_options("m".into());
        assert!(opts.ignore_closed);
        assert!(!opts.allow_closed);
    }

    #[test]
    fn route_rejects_ignore_closed_with_allow_closed() {
        let Err(err) = parse(&["#1", "--ignore-closed", "--allow-closed"]) else {
            panic!("expected a conflict"); // omni-dev: coverage ignore-line reason="guards this test's assumption; the parse above always fails on --ignore-closed with --allow-closed"
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
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
            ClassSource, IssueRoute, ProviderRoute, RouteOutcome, StageAnswer, StageAnswers,
        };
        let answer = || StageAnswer {
            effort_by_model: BTreeMap::new(),
            choice: "sonnet".to_string(),
            confidence: 0.9,
            probabilities: BTreeMap::new(),
        };
        let issue = |outcome| IssueRoute {
            draft_comments: vec![],
            item_ref: "o/r#1".to_string(),
            url: String::new(),
            title: String::new(),
            state: ItemState::Open,
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
                        class_from: ClassSource::Implement,
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
                reference_fetch_failures: vec![],
            })
        };
        let report = |issues| RouteReport {
            model: String::new(),
            issues,
            ignored_closed: vec![],
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
            ClassSource, IssueRoute, ProviderRoute, ReferenceFetchFailure, RouteOutcome,
            StageAnswer, StageAnswers,
        };
        let answer = || StageAnswer {
            effort_by_model: BTreeMap::new(),
            choice: "sonnet".to_string(),
            confidence: 0.9,
            probabilities: BTreeMap::new(),
        };
        let report = RouteReport {
            model: "jev-1.13.0".to_string(),
            issues: vec![IssueRoute {
                draft_comments: vec![],
                item_ref: "o/r#1".to_string(),
                url: "u".to_string(),
                title: "t".to_string(),
                state: ItemState::Open,
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
                            class_from: ClassSource::Implement,
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
            ignored_closed: vec![],
            usage: Usage::default(),
        };

        let json =
            render_output(&report, RouteFormat::Json, DEFAULT_MAX_INPUT_CHARS, None).unwrap();
        assert!(json.contains("\"model\": \"jev-1.13.0\""), "{json}");
        assert!(json.contains("\"reference_fetch_failures\""), "{json}");
        assert!(json.contains("\"ref\": \"#404\""), "{json}");

        let yaml =
            render_output(&report, RouteFormat::Yaml, DEFAULT_MAX_INPUT_CHARS, None).unwrap();
        assert!(yaml.contains("model: jev-1.13.0"), "{yaml}");
        assert!(yaml.contains("reference_fetch_failures:"), "{yaml}");
        assert!(yaml.contains("ref: '#404'"), "{yaml}");

        let usage = Some(CacheUsage {
            items_reused: 2,
            oldest_age_secs: 90,
        });
        let cached =
            render_output(&report, RouteFormat::Json, DEFAULT_MAX_INPUT_CHARS, usage).unwrap();
        assert!(cached.contains("\"github_cache\""), "{cached}");
        assert!(!json.contains("github_cache"), "{json}");
        let cached_text =
            render_output(&report, RouteFormat::Text, DEFAULT_MAX_INPUT_CHARS, usage).unwrap();
        assert!(!cached_text.contains("github_cache"), "{cached_text}");

        let text =
            render_output(&report, RouteFormat::Text, DEFAULT_MAX_INPUT_CHARS, None).unwrap();
        assert!(text.starts_with("o/r#1 — t\n"), "{text}");
        assert!(
            text.contains("reference fetch failed: #404 (not found)"),
            "{text}"
        );

        let style = TerminalStyle {
            color: true,
            hyperlinks: true,
        };
        for format in [RouteFormat::Json, RouteFormat::Yaml] {
            assert_eq!(
                render_output_with_style(
                    &report,
                    format,
                    DEFAULT_MAX_INPUT_CHARS,
                    &[],
                    style,
                    None
                )
                .unwrap(),
                render_output(&report, format, DEFAULT_MAX_INPUT_CHARS, None).unwrap()
            );
        }

        let styled_text = render_output_with_style(
            &report,
            RouteFormat::Text,
            DEFAULT_MAX_INPUT_CHARS,
            &[],
            style,
            None,
        )
        .unwrap();
        assert_eq!(
            styled_text,
            render_route_text_styled(&report, DEFAULT_MAX_INPUT_CHARS, &[], style)
        );
        assert_ne!(
            styled_text, text,
            "styled text should differ from the plain rendering"
        );
    }

    // ── terminal_style_with (env seam) ───────────────────────────────

    #[test]
    fn terminal_style_reads_colour_and_link_support_from_the_environment() {
        use crate::test_support::env::MapEnv;

        let xterm = MapEnv::new().with("TERM", "xterm-256color");
        assert_eq!(
            terminal_style_with(&xterm, true),
            TerminalStyle {
                color: true,
                hyperlinks: false
            }
        );
        assert_eq!(terminal_style_with(&xterm, false), TerminalStyle::default());
        assert!(!terminal_style_with(&xterm.clone().with("NO_COLOR", "1"), true).color);
        // An empty `NO_COLOR` is "unset" per no-color.org.
        assert!(terminal_style_with(&xterm.clone().with("NO_COLOR", ""), true).color);
        for (key, value) in [
            ("TERM_PROGRAM", "ghostty"),
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM", "WezTerm"),
            ("TERM_PROGRAM", "vscode"),
            ("TERM_PROGRAM", "Hyper"),
            ("WT_SESSION", "1"),
            ("KITTY_WINDOW_ID", "1"),
            ("KONSOLE_VERSION", "230804"),
            ("ALACRITTY_WINDOW_ID", "1"),
            ("VTE_VERSION", "6003"),
        ] {
            assert!(
                terminal_style_with(&xterm.clone().with(key, value), true).hyperlinks,
                "{key}={value}"
            );
        }
        for (key, value) in [
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("VTE_VERSION", "4602"),
            ("VTE_VERSION", "not a number"),
        ] {
            assert!(
                !terminal_style_with(&xterm.clone().with(key, value), true).hyperlinks,
                "{key}={value}"
            );
        }
        // A capable terminal still emits nothing when stdout is redirected.
        assert_eq!(
            terminal_style_with(&xterm.with("TERM_PROGRAM", "ghostty"), false),
            TerminalStyle::default()
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
                &IssueCache::disabled(),
                dir.path(),
                &["rust-works/omni-dev#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        assert_eq!(docs.len(), 1);
        assert!(deps.is_empty());
        assert_eq!(calls(dir.path()), ["api"]);
    }

    #[test]
    fn fetch_docs_reuses_a_cached_issue_on_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let args = ["rust-works/omni-dev#1".to_string()];
        let mut notes = Vec::new();
        for _ in 0..2 {
            // Each run builds its own cache over the same directory.
            let cache = IssueCache::new(
                cache_dir.path().to_path_buf(),
                crate::github_issues::DEFAULT_CACHE_TTL,
                false,
            );
            let (docs, _deps, _failures) = retry_on_etxtbsy(|| {
                fetch_docs(
                    &bin,
                    &cache,
                    dir.path(),
                    &args,
                    false,
                    DEFAULT_MAX_INPUT_CHARS,
                    false,
                    &[],
                )
            })
            .unwrap();
            assert_eq!(docs.len(), 1);
            notes.push(cache.reuse_note());
        }
        // One full fetch on the cold run, then only the shallow state recheck
        // (#2041): the cached text is served, but its state is validated.
        assert_eq!(calls(dir.path()), ["api", "api"]);
        assert!(notes[0].is_none(), "a cold run reuses nothing");
        assert!(notes[1].is_some());
    }

    #[test]
    fn fetch_docs_resolves_bare_numbers_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh(dir.path());
        let args = ["#1", "1", "rust-works/omni-dev#2"].map(str::to_string);
        let (docs, _deps, _failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &IssueCache::disabled(),
                dir.path(),
                &args,
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
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
        let (docs, _deps, _failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &IssueCache::disabled(),
                dir.path(),
                &[],
                true,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        let numbers: Vec<u64> = docs.iter().map(|d| d.number).collect();
        assert_eq!(numbers, [5, 6]);
        assert_eq!(calls(dir.path()), ["repo", "issue", "api"]);
    }

    #[test]
    fn fetch_docs_explains_a_failed_repo_lookup() {
        let err = fetch_docs(
            Path::new("/no/such/gh/xyzzy"),
            &IssueCache::disabled(),
            Path::new("."),
            &["#1".to_string()],
            false,
            DEFAULT_MAX_INPUT_CHARS,
            false,
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("-C/--repo"), "{err}");
    }

    // ── fetch_docs cached state (#2041) ──────────────────────────────

    /// A fake `gh` for which every issue is in `state`, answering both the full
    /// fetch and the `state`-only recheck, and logging each call's kind.
    fn fake_gh_at_state(dir: &Path, state: &str) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let issue = serde_json::json!({
            "title": "fresh", "body": "b", "state": state, "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let full = serde_json::json!({"data": {"r0": {"i0": issue, "i1": issue}}});
        let only = serde_json::json!({"__typename": "Issue", "state": state});
        let shallow = serde_json::json!({"data": {"r0": {"i0": only, "i1": only}}});
        let path = dir.join("fake-gh");
        let calls = dir.join("calls");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 repo) echo repo >> '{calls}'; echo rust-works/omni-dev ;;\n\
                 issue) echo issue >> '{calls}'; echo '[{{\"number\": 5}}, {{\"number\": 6}}]' ;;\n\
                 *) case \"$4\" in\n\
                    *'on Issue {{ state }}'*) echo state >> '{calls}'; cat <<'JSON'\n{shallow}\nJSON\n;;\n\
                    *) echo full >> '{calls}'; cat <<'JSON'\n{full}\nJSON\n;;\n\
                    esac ;;\n\
                 esac\n",
                calls = calls.display()
            ),
        );
        (path, guard)
    }

    /// Warms `cache_dir` with issues #5 and #6 as GitHub reports them in
    /// `state`, then forgets the run's call log.
    fn warm_cache_at_state(dir: &Path, cache_dir: &Path, state: &str) {
        let (bin, _shim) = fake_gh_at_state(dir, state);
        let cache = IssueCache::new(
            cache_dir.to_path_buf(),
            crate::github_issues::DEFAULT_CACHE_TTL,
            false,
        );
        retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &cache,
                dir,
                &[
                    "rust-works/omni-dev#5".to_string(),
                    "rust-works/omni-dev#6".to_string(),
                ],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        std::fs::remove_file(dir.join("calls")).unwrap();
    }

    fn fresh_cache(cache_dir: &Path) -> IssueCache {
        IssueCache::new(
            cache_dir.to_path_buf(),
            crate::github_issues::DEFAULT_CACHE_TTL,
            false,
        )
    }

    /// The listing is fresh, so an issue cached as closed but listed as open
    /// (it was reopened) is refetched rather than skipped as closed.
    #[test]
    fn fetch_docs_all_open_refetches_a_reopened_issue_cached_as_closed() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        warm_cache_at_state(dir.path(), cache_dir.path(), "CLOSED");
        let (bin, _shim) = fake_gh_at_state(dir.path(), "OPEN");
        let (docs, _deps, _failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &fresh_cache(cache_dir.path()),
                dir.path(),
                &[],
                true,
                DEFAULT_MAX_INPUT_CHARS,
                true,
                &[],
            )
        })
        .unwrap();
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().all(|d| d.state == ItemState::Open), "{docs:?}");
        assert!(docs.iter().all(|d| !is_ignored_closed(d, true)));
        // The listing proved the state, so there is no shallow recheck.
        assert_eq!(calls(dir.path()), ["repo", "issue", "full"]);
    }

    #[test]
    fn fetch_docs_all_open_reuses_a_cached_open_issue_without_a_recheck() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        warm_cache_at_state(dir.path(), cache_dir.path(), "OPEN");
        let (bin, _shim) = fake_gh_at_state(dir.path(), "OPEN");
        let cache = fresh_cache(cache_dir.path());
        let (docs, _deps, _failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &cache,
                dir.path(),
                &[],
                true,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(calls(dir.path()), ["repo", "issue"]);
        assert!(cache.reuse_note().is_some());
    }

    /// An explicit issue cached as open but closed since is refetched, so
    /// `--ignore-closed` drops it instead of routing a closed issue.
    #[test]
    fn fetch_docs_refetches_an_explicit_issue_closed_since_it_was_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        warm_cache_at_state(dir.path(), cache_dir.path(), "OPEN");
        let (bin, _shim) = fake_gh_at_state(dir.path(), "CLOSED");
        let (docs, _deps, _failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &fresh_cache(cache_dir.path()),
                dir.path(),
                &["rust-works/omni-dev#5".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                true,
                &[],
            )
        })
        .unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].state, ItemState::Closed);
        assert!(is_ignored_closed(&docs[0], true));
        assert_eq!(calls(dir.path()), ["state", "full"]);
    }

    /// A fake `gh` whose issue `#1` (open) cites `#2`, which is in
    /// `cited_state` for both the full fetch and the `state`-only recheck.
    fn fake_gh_citing_at(
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
            "title": "t2", "body": "b2", "state": cited_state,
            "url": "https://github.com/rust-works/omni-dev/issues/2"
        });
        let open = serde_json::json!({"__typename": "Issue", "state": "OPEN"});
        let closed_since = serde_json::json!({"__typename": "Issue", "state": cited_state});
        let path = dir.join("fake-gh");
        write_exec_script(
            &path,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 repo) echo rust-works/omni-dev ;;\n\
                 *) case \"$4\" in\n\
                    *'number:2'*'on Issue {{ state }}'*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {closed_since}}}}}}}\nJSON\n;;\n\
                    *'on Issue {{ state }}'*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {open}}}}}}}\nJSON\n;;\n\
                    *'number:2'*) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {cited}}}}}}}\nJSON\n;;\n\
                    *) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {citing}}}}}}}\nJSON\n;;\n\
                    esac ;;\n\
                 esac\n",
            ),
        );
        (path, guard)
    }

    /// A cited issue cached as open and closed since is not an open
    /// dependency, even though its cached copy still says so.
    #[test]
    fn fetch_docs_drops_a_citation_closed_since_it_was_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let args = ["rust-works/omni-dev#1".to_string()];
        let run = |bin: &Path| {
            retry_on_etxtbsy(|| {
                fetch_docs(
                    bin,
                    &fresh_cache(cache_dir.path()),
                    dir.path(),
                    &args,
                    false,
                    DEFAULT_MAX_INPUT_CHARS,
                    false,
                    &[],
                )
            })
            .unwrap()
        };
        {
            let (bin, _shim) = fake_gh_citing_at(dir.path(), "OPEN");
            let (_docs, deps, _failures) = run(&bin);
            assert_eq!(deps.len(), 1, "the warm run sees an open dependency");
        }
        let (bin, _shim) = fake_gh_citing_at(dir.path(), "CLOSED");
        let (_docs, deps, _failures) = run(&bin);
        assert!(deps.is_empty(), "{deps:?}");
    }

    // ── fetch_docs dependencies (#1812) ────────────────────────────────

    /// A fake `gh` whose sole issue (`#1`) cites a second, distinct issue
    /// (`#2`) — open or closed depending on `cited_state`.
    fn fake_gh_with_citation(
        dir: &Path,
        cited_state: &str,
    ) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        fake_gh_with_states(dir, "OPEN", cited_state)
    }

    /// [`fake_gh_with_citation`] with the citing issue's own state chosen too.
    fn fake_gh_with_states(
        dir: &Path,
        citing_state: &str,
        cited_state: &str,
    ) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let citing = serde_json::json!({
            "__typename": "Issue",
            "title": "t", "body": "see #2", "state": citing_state, "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let cited = serde_json::json!({
            "__typename": "Issue",
            "title": "t2", "body": "b2", "state": cited_state,
            "url": "https://github.com/rust-works/omni-dev/issues/2"
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
                &IssueCache::disabled(),
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        let open = deps
            .get(&(docs[0].project.clone(), docs[0].number))
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].raw, "#2");
        assert_eq!(
            open[0].url.as_deref(),
            Some("https://github.com/rust-works/omni-dev/issues/2")
        );
    }

    #[test]
    fn fetch_docs_drops_a_closed_citation() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_citation(dir.path(), "CLOSED");
        let (_docs, deps, _failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &IssueCache::disabled(),
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        assert!(deps.is_empty(), "{deps:?}");
    }

    /// A closed issue `--ignore-closed` will skip keeps its place in the
    /// fetched docs (the engine lists it under `ignored_closed`), but its
    /// citations are not resolved (#2000).
    #[test]
    fn fetch_docs_skips_citations_of_a_closed_issue_when_ignoring_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_states(dir.path(), "CLOSED", "OPEN");
        let fetch = |ignore_closed| {
            retry_on_etxtbsy(|| {
                fetch_docs(
                    &bin,
                    &IssueCache::disabled(),
                    dir.path(),
                    &["rust-works/omni-dev#1".to_string()],
                    false,
                    DEFAULT_MAX_INPUT_CHARS,
                    ignore_closed,
                    &[],
                )
            })
            .unwrap()
        };
        let (docs, deps, failures) = fetch(true);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].state, ItemState::Closed);
        assert!(deps.is_empty(), "{deps:?}");
        assert!(failures.is_empty(), "{failures:?}");

        let (_docs, deps, _failures) = fetch(false);
        assert_eq!(deps.len(), 1, "{deps:?}");
    }

    #[test]
    fn fetch_docs_reports_a_missing_citation_as_a_reference_fetch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, _shim) = fake_gh_with_missing_citation(dir.path());
        let (docs, dependencies, failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &IssueCache::disabled(),
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
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

    /// #2001: an issue quoting `owner/repo#123` (say, in an error message)
    /// cites a repository that doesn't exist. GitHub answers with a
    /// repository-level `NOT_FOUND`; routing must report it as a reference
    /// fetch failure rather than abort.
    #[test]
    fn fetch_docs_reports_a_citation_into_a_missing_repository_as_a_reference_fetch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let _shim = shim_lock();
        let citing = serde_json::json!({
            "__typename": "Issue",
            "title": "t",
            "body": "Error: gh api graphql failed: see owner/repo#123",
            "state": "OPEN", "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let bin = dir.path().join("fake-gh");
        write_exec_script(
            &bin,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 repo) echo rust-works/omni-dev ;;\n\
                 *) case \"$4\" in\n\
                    *'number:123'*) cat <<'JSON'\n{{\"data\": {{\"r0\": null}}, \"errors\": [{{\"type\": \"NOT_FOUND\", \"path\": [\"r0\"], \"message\": \"Could not resolve to a Repository with the name 'owner/repo'.\"}}]}}\nJSON\nexit 1 ;;\n\
                    *) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {citing}}}}}}}\nJSON\n;;\n\
                    esac ;;\n\
                 esac\n",
            ),
        );
        let (docs, dependencies, failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &IssueCache::disabled(),
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        let key = (docs[0].project.clone(), docs[0].number);
        assert!(dependencies.is_empty(), "{dependencies:?}");
        assert_eq!(
            failures.get(&key),
            Some(&vec![ReferenceFetchFailure {
                item_ref: "owner/repo#123".to_string(),
                error: "not found".to_string(),
            }])
        );
    }

    /// #2003: the same quoted example inside a fenced code block or an inline
    /// span is not a citation at all, so it neither triggers a lookup (the
    /// shim would answer it with `NOT_FOUND`) nor lands in
    /// `reference_fetch_failures`.
    #[test]
    fn fetch_docs_ignores_a_citation_quoted_in_code() {
        let dir = tempfile::tempdir().unwrap();
        let _shim = shim_lock();
        let citing = serde_json::json!({
            "__typename": "Issue",
            "title": "t",
            "body": "```\nError: gh api graphql failed: see owner/repo#123\n```\nand `o/r#1` inline",
            "state": "OPEN", "url": "u",
            "comments": {"totalCount": 0, "nodes": []},
            "closedByPullRequestsReferences": {"nodes": []}
        });
        let bin = dir.path().join("fake-gh");
        write_exec_script(
            &bin,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 repo) echo rust-works/omni-dev ;;\n\
                 *) case \"$4\" in\n\
                    *'number:123'*) cat <<'JSON'\n{{\"data\": {{\"r0\": null}}, \"errors\": [{{\"type\": \"NOT_FOUND\", \"path\": [\"r0\"], \"message\": \"Could not resolve to a Repository with the name 'owner/repo'.\"}}]}}\nJSON\nexit 1 ;;\n\
                    *) cat <<'JSON'\n{{\"data\": {{\"r0\": {{\"i0\": {citing}}}}}}}\nJSON\n;;\n\
                    esac ;;\n\
                 esac\n",
            ),
        );
        let (docs, dependencies, failures) = retry_on_etxtbsy(|| {
            fetch_docs(
                &bin,
                &IssueCache::disabled(),
                dir.path(),
                &["#1".to_string()],
                false,
                DEFAULT_MAX_INPUT_CHARS,
                false,
                &[],
            )
        })
        .unwrap();
        let key = (docs[0].project.clone(), docs[0].number);
        assert!(dependencies.is_empty(), "{dependencies:?}");
        assert!(!failures.contains_key(&key), "{failures:?}");
    }
}
