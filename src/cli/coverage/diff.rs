//! `omni-dev coverage diff` — diff/patch coverage analysis.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use git2::Repository;
use regex::RegexSet;

use crate::coverage::{
    analyze, default_base_ref, parse, render, DiffModel, DiffScope, Format, OutputFormat,
    RenderOptions,
};

/// Coverage report format selector (CLI mirror of [`Format`] plus auto-detect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ReportFormat {
    /// Auto-detect from report content.
    Auto,
    /// lcov trace file.
    Lcov,
    /// llvm-cov JSON export (`cargo llvm-cov report --json`).
    LlvmCovJson,
    /// Cobertura XML.
    Cobertura,
}

impl ReportFormat {
    /// Converts to a concrete [`Format`], or `None` for auto-detection.
    fn into_format(self) -> Option<Format> {
        match self {
            Self::Auto => None,
            Self::Lcov => Some(Format::Lcov),
            Self::LlvmCovJson => Some(Format::LlvmCovJson),
            Self::Cobertura => Some(Format::Cobertura),
        }
    }
}

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum OutputFormatArg {
    /// Markdown PR comment (default).
    Markdown,
    /// YAML structured output.
    Yaml,
    /// JSON output.
    Json,
}

impl From<OutputFormatArg> for OutputFormat {
    fn from(arg: OutputFormatArg) -> Self {
        match arg {
            OutputFormatArg::Markdown => Self::Markdown,
            OutputFormatArg::Yaml => Self::Yaml,
            OutputFormatArg::Json => Self::Json,
        }
    }
}

/// Analyses diff/patch coverage from a per-line report and a git diff.
#[derive(Parser)]
pub struct DiffCommand {
    /// Head coverage report (lcov / llvm-cov-json / cobertura).
    #[arg(long, value_name = "PATH")]
    pub report: PathBuf,

    /// Format of `--report` (auto-detected by default).
    #[arg(long, value_enum, default_value_t = ReportFormat::Auto)]
    pub report_format: ReportFormat,

    /// Base revision to diff against (default: merge-base of `origin/main` and `HEAD`).
    #[arg(long, value_name = "REV")]
    pub base_ref: Option<String>,

    /// Head revision the report was measured at (default: `HEAD`).
    #[arg(long, value_name = "REV")]
    pub head_ref: Option<String>,

    /// Optional baseline coverage report; enables project deltas and indirect-change detection.
    #[arg(long, value_name = "PATH")]
    pub baseline_report: Option<PathBuf>,

    /// Format of `--baseline-report` (auto-detected by default).
    #[arg(long, value_enum, default_value_t = ReportFormat::Auto)]
    pub baseline_report_format: ReportFormat,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormatArg::Markdown)]
    pub output: OutputFormatArg,

    /// Deprecated: use `-o`/`--output` instead.
    #[arg(long = "format", hide = true)]
    pub format: Option<OutputFormatArg>,

    /// Fail (non-zero exit) when patch coverage is below this percentage.
    #[arg(long, value_name = "PCT")]
    pub fail_under_patch: Option<f64>,

    /// Override the path prefix stripped from report file paths to make them
    /// repo-relative (default: the repository working directory).
    #[arg(long, value_name = "PATH")]
    pub strip_prefix: Option<PathBuf>,

    /// Exclude files whose repo-relative path matches any of these regexes
    /// from BOTH the head and baseline reports before computing the diff.
    ///
    /// Repeatable, or comma-separated. Matching is unanchored (partial), the
    /// same semantics as `cargo llvm-cov --ignore-filename-regex`, and is
    /// applied after `--strip-prefix` normalisation so the pattern matches the
    /// repo-relative path. Filtering both sides symmetrically keeps the total,
    /// per-file deltas, patch coverage, and indirect-change list — and the
    /// `--fail-under-patch` gate — computed over the same denominator, even
    /// when the baseline predates the exclusion.
    #[arg(long, value_name = "REGEX", value_delimiter = ',')]
    pub ignore_filename_regex: Vec<String>,

    /// Collapse consecutive uncovered new lines into ranges (e.g. `9-11`).
    #[arg(long)]
    pub collapse_ranges: bool,

    /// Report per-file deltas and indirect changes for ALL files, not just the
    /// ones this diff touches.
    ///
    /// By default the project-delta and indirect-change sections are scoped to
    /// files the diff modifies, because coverage is measured by two independent
    /// test runs and lines in untouched files flip purely from run-to-run
    /// variance. This flag restores the unscoped (noisier) report.
    #[arg(long)]
    pub all_files: bool,

    /// Link to the full coverage-summary artifact (markdown footer).
    #[arg(long, value_name = "URL")]
    pub artifact_url: Option<String>,

    /// Link to the CI run (markdown footer).
    #[arg(long, value_name = "URL")]
    pub run_url: Option<String>,

    /// Base (merge-base) commit SHA shown in the markdown `Comparing` line.
    #[arg(long, value_name = "SHA")]
    pub base_sha: Option<String>,

    /// Head commit SHA shown in the markdown `Comparing` line.
    #[arg(long, value_name = "SHA")]
    pub head_sha: Option<String>,

    /// Commit-URL prefix for linking SHAs (e.g. `https://…/<repo>/commit`).
    #[arg(long, value_name = "URL")]
    pub commit_url: Option<String>,
}

/// The result of running `coverage diff`, separated from printing so it can be
/// exercised by tests and reused programmatically.
pub struct DiffOutcome {
    /// The rendered report in the requested format.
    pub rendered: String,
    /// Project-wide patch coverage percentage (`None` when no lines were added).
    pub patch_percent: Option<f64>,
    /// Whether `--fail-under-patch` was set and patch coverage fell below it.
    pub below_gate: bool,
}

impl DiffCommand {
    /// Executes the command: prints the report and applies the patch gate.
    ///
    /// `repo` is the repository location resolved at the CLI boundary
    /// (`None` = current working directory).
    pub async fn execute(mut self, repo: Option<&Path>) -> Result<()> {
        if let Some(format) = self.format.take() {
            eprintln!("warning: --format is deprecated; use -o/--output instead");
            self.output = format;
        }
        let outcome = self.run(repo)?;
        println!("{}", outcome.rendered);
        if outcome.below_gate {
            let pct = outcome.patch_percent.unwrap_or(0.0);
            anyhow::bail!(
                "patch coverage {pct:.2}% is below the --fail-under-patch threshold of {:.2}%",
                self.fail_under_patch.unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Runs the analysis and renders the output without printing.
    ///
    /// `repo_root` is the repository to analyze (`None` defaults to `.`, which
    /// preserves the CI invocation that runs from the repo root). Relative
    /// `--report`/`--baseline-report` paths are anchored to it so the git repo
    /// and the coverage reports always resolve against the same root.
    pub fn run(&self, repo_root: Option<&Path>) -> Result<DiffOutcome> {
        let repo_path = repo_root.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let repo = Repository::open(&repo_path)
            .with_context(|| format!("could not open git repository at {}", repo_path.display()))?;

        // Resolve the base ref (default: merge-base of origin/main and HEAD).
        let base_ref = match &self.base_ref {
            Some(r) => r.clone(),
            None => default_base_ref(&repo)?,
        };

        // Determine the prefix stripped from report paths to make them repo-relative.
        let strip_prefix = self
            .strip_prefix
            .clone()
            .or_else(|| repo.workdir().map(std::path::Path::to_path_buf));

        // Compiled once so an invalid pattern is a single up-front error rather
        // than failing separately per report.
        let ignore = self.compile_ignore()?;

        let head = self.load_report(
            &self.report,
            self.report_format,
            strip_prefix.as_deref(),
            ignore.as_ref(),
            &repo_path,
        )?;
        let baseline = match &self.baseline_report {
            Some(path) => Some(self.load_report(
                path,
                self.baseline_report_format,
                strip_prefix.as_deref(),
                ignore.as_ref(),
                &repo_path,
            )?),
            None => None,
        };

        let diff = DiffModel::between(&repo, &base_ref, self.head_ref.as_deref())?;
        let scope = if self.all_files {
            DiffScope::All
        } else {
            DiffScope::DiffOnly
        };
        let result = analyze(&head, &diff, baseline.as_ref(), scope);

        let opts = self.render_options();
        let rendered = render(&result, &opts, self.output.into())?;

        let patch_percent = result.patch.percent();
        let below_gate = match self.fail_under_patch {
            // No added lines ⇒ nothing to gate on; treat as a pass.
            Some(threshold) => patch_percent.is_some_and(|p| p < threshold),
            None => false,
        };

        Ok(DiffOutcome {
            rendered,
            patch_percent,
            below_gate,
        })
    }

    /// Reads and parses a coverage report, normalising paths to be repo-relative.
    ///
    /// A relative `path` is resolved against `repo_root` so the report and the
    /// git repository always anchor to the same root; an absolute `path` is
    /// used as-is.
    fn load_report(
        &self,
        path: &std::path::Path,
        format: ReportFormat,
        strip_prefix: Option<&std::path::Path>,
        ignore: Option<&RegexSet>,
        repo_root: &Path,
    ) -> Result<crate::coverage::CoverageReport> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            repo_root.join(path)
        };
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read coverage report {}", path.display()))?;
        let mut report = parse(&content, format.into_format())
            .with_context(|| format!("could not parse coverage report {}", path.display()))?;
        if let Some(prefix) = strip_prefix {
            report.strip_prefix(prefix);
        }
        // Match on the repo-relative path (post strip-prefix), so the same
        // pattern applies identically to head and baseline.
        if let Some(ignore) = ignore {
            report.retain_paths(|path| !ignore.is_match(path));
        }
        Ok(report)
    }

    /// Compiles `--ignore-filename-regex` into a [`RegexSet`], or `None` when it
    /// was not supplied. A file is excluded when it matches *any* pattern.
    ///
    /// Empty patterns are dropped: comma-splitting a trailing or doubled comma
    /// (`foo,` / `a,,b`) yields an empty element, and an empty regex matches
    /// *every* path — which would silently exclude the whole report (and make a
    /// `--fail-under-patch` gate pass vacuously). Treating it as a no-op is the
    /// safe reading of an obvious typo.
    fn compile_ignore(&self) -> Result<Option<RegexSet>> {
        let patterns: Vec<&String> = self
            .ignore_filename_regex
            .iter()
            .filter(|p| !p.is_empty())
            .collect();
        if patterns.is_empty() {
            return Ok(None);
        }
        let set = RegexSet::new(patterns).context("invalid --ignore-filename-regex pattern")?;
        Ok(Some(set))
    }

    /// Builds the render options, falling back to the `COVERAGE_*` environment
    /// variables CI sets when a flag is not supplied.
    fn render_options(&self) -> RenderOptions {
        fn or_env(flag: &Option<String>, var: &str) -> Option<String> {
            flag.clone()
                .or_else(|| std::env::var(var).ok())
                .filter(|s| !s.is_empty())
        }
        RenderOptions {
            artifact_url: or_env(&self.artifact_url, "COVERAGE_ARTIFACT_URL"),
            run_url: or_env(&self.run_url, "COVERAGE_RUN_URL"),
            base_sha: or_env(&self.base_sha, "COVERAGE_BASE_SHA"),
            head_sha: or_env(&self.head_sha, "COVERAGE_HEAD_SHA"),
            commit_url: or_env(&self.commit_url, "COVERAGE_COMMIT_URL"),
            collapse_ranges: self.collapse_ranges,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Creates a temp repo with a base commit (`a.rs`) and a head commit that
    /// adds `b.rs` with three lines. Returns the dir, repo path, and base SHA.
    fn repo_with_added_file() -> (TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let repo = Repository::init(&path).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }

        let commit = |repo: &Repository, files: &[(&str, &str)], parent: Option<git2::Oid>| {
            let mut index = repo.index().unwrap();
            index.clear().unwrap();
            for (name, content) in files {
                fs::write(path.join(name), content).unwrap();
                index.add_path(Path::new(name)).unwrap();
            }
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = Signature::now("Test", "test@example.com").unwrap();
            let parent = parent.map(|id| repo.find_commit(id).unwrap());
            let parents: Vec<&git2::Commit> = parent.as_ref().into_iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &parents)
                .unwrap()
        };

        let base = commit(&repo, &[("a.rs", "fn a() {}\n")], None);
        commit(
            &repo,
            &[("a.rs", "fn a() {}\n"), ("b.rs", "one\ntwo\nthree\n")],
            Some(base),
        );
        // Return git2's canonical workdir: on macOS the tempdir `/var/...` is a
        // symlink to `/private/var/...`, and `Repository::open` resolves to the
        // latter. Using it for both the repo path and the report's `SF:` path
        // keeps `strip_prefix` (which defaults to the workdir) consistent.
        let workdir = repo.workdir().unwrap().to_path_buf();
        (dir, workdir, base.to_string())
    }

    /// Writes an lcov report for `b.rs` (line 1 & 3 covered, line 2 uncovered).
    fn write_head_lcov(repo_path: &Path) -> PathBuf {
        let lcov = format!(
            "SF:{}\nDA:1,1\nDA:2,0\nDA:3,4\nend_of_record\n",
            repo_path.join("b.rs").display()
        );
        let report = repo_path.join("head.lcov");
        fs::write(&report, lcov).unwrap();
        report
    }

    /// Builds a `DiffCommand` with defaults pointed at the given report. The
    /// repository root is supplied separately to `run`/`execute`.
    fn command(report: PathBuf, base_ref: &str) -> DiffCommand {
        DiffCommand {
            report,
            report_format: ReportFormat::Auto,
            base_ref: Some(base_ref.to_string()),
            head_ref: None,
            baseline_report: None,
            baseline_report_format: ReportFormat::Auto,
            output: OutputFormatArg::Markdown,
            format: None,
            fail_under_patch: None,
            strip_prefix: None,
            ignore_filename_regex: Vec::new(),
            collapse_ranges: false,
            all_files: false,
            artifact_url: None,
            run_url: None,
            base_sha: None,
            head_sha: None,
            commit_url: None,
        }
    }

    #[test]
    fn report_format_into_format() {
        assert_eq!(ReportFormat::Auto.into_format(), None);
        assert_eq!(ReportFormat::Lcov.into_format(), Some(Format::Lcov));
        assert_eq!(
            ReportFormat::LlvmCovJson.into_format(),
            Some(Format::LlvmCovJson)
        );
        assert_eq!(
            ReportFormat::Cobertura.into_format(),
            Some(Format::Cobertura)
        );
    }

    #[test]
    fn output_format_arg_conversion() {
        assert_eq!(
            OutputFormat::from(OutputFormatArg::Markdown),
            OutputFormat::Markdown
        );
        assert_eq!(
            OutputFormat::from(OutputFormatArg::Yaml),
            OutputFormat::Yaml
        );
        assert_eq!(
            OutputFormat::from(OutputFormatArg::Json),
            OutputFormat::Json
        );
    }

    #[test]
    fn run_markdown_reports_patch_coverage() {
        let (_dir, repo, base) = repo_with_added_file();
        let report = write_head_lcov(&repo);
        let outcome = command(report, &base).run(Some(&repo)).unwrap();
        // 3 added lines, 2 covered.
        assert_eq!(outcome.patch_percent, Some(2.0 / 3.0 * 100.0));
        assert!(!outcome.below_gate);
        assert!(outcome.rendered.contains("### Patch coverage"));
        assert!(outcome.rendered.contains("`b.rs:2`"));
    }

    #[test]
    fn run_yaml_and_json_formats() {
        let (_dir, repo, base) = repo_with_added_file();
        for format in [OutputFormatArg::Yaml, OutputFormatArg::Json] {
            let report = write_head_lcov(&repo);
            let mut cmd = command(report, &base);
            cmd.output = format;
            let outcome = cmd.run(Some(&repo)).unwrap();
            assert!(outcome.rendered.contains("patch_coverage"));
        }
    }

    #[test]
    fn run_with_baseline_enables_delta() {
        let (_dir, repo, base) = repo_with_added_file();
        let report = write_head_lcov(&repo);
        // Baseline only knows a.rs at 100%.
        let baseline = repo.join("base.lcov");
        fs::write(
            &baseline,
            format!("SF:{}/a.rs\nDA:1,1\nend_of_record\n", repo.display()),
        )
        .unwrap();
        let mut cmd = command(report, &base);
        cmd.baseline_report = Some(baseline);
        let outcome = cmd.run(Some(&repo)).unwrap();
        assert!(outcome.rendered.contains("vs `main`"));
    }

    #[test]
    fn fail_under_patch_gate() {
        let (_dir, repo, base) = repo_with_added_file();
        // Patch coverage is ~66.7%.
        let report = write_head_lcov(&repo);
        let mut cmd = command(report, &base);
        cmd.fail_under_patch = Some(90.0);
        assert!(
            cmd.run(Some(&repo)).unwrap().below_gate,
            "66.7% < 90% should fail"
        );

        let report = write_head_lcov(&repo);
        let mut cmd = command(report, &base);
        cmd.fail_under_patch = Some(50.0);
        assert!(
            !cmd.run(Some(&repo)).unwrap().below_gate,
            "66.7% >= 50% should pass"
        );
    }

    #[test]
    fn all_files_scope_surfaces_untouched_file() {
        // `a.rs` is unchanged between base and head, so the diff never touches
        // it. Its coverage moves by a single line (2/4 → 3/4): a small enough
        // net move that DiffOnly drops it as noise, but a 25 pp shift that the
        // delta table renders once `--all-files` widens the scope to `All`.
        let (_dir, repo, base) = repo_with_added_file();
        let head = format!(
            "SF:{a}\nDA:1,1\nDA:2,1\nDA:3,1\nDA:4,0\nend_of_record\n\
             SF:{b}\nDA:1,1\nDA:2,0\nDA:3,4\nend_of_record\n",
            a = repo.join("a.rs").display(),
            b = repo.join("b.rs").display(),
        );
        let report = repo.join("head.lcov");
        fs::write(&report, head).unwrap();
        let baseline = repo.join("base.lcov");
        fs::write(
            &baseline,
            format!(
                "SF:{}\nDA:1,1\nDA:2,1\nDA:3,0\nDA:4,0\nend_of_record\n",
                repo.join("a.rs").display()
            ),
        )
        .unwrap();

        // Default (DiffOnly): the untouched `a.rs` row is filtered out as noise.
        let mut scoped = command(report.clone(), &base);
        scoped.baseline_report = Some(baseline.clone());
        let scoped_md = scoped.run(Some(&repo)).unwrap().rendered;
        assert!(
            !scoped_md.contains("`a.rs`"),
            "DiffOnly must hide untouched a.rs"
        );

        // --all-files (All): the untouched `a.rs` row is now surfaced.
        let mut all = command(report, &base);
        all.baseline_report = Some(baseline);
        all.all_files = true;
        let all_md = all.run(Some(&repo)).unwrap().rendered;
        assert!(
            all_md.contains("`a.rs`"),
            "All scope must surface untouched a.rs"
        );
    }

    #[test]
    fn ignore_filename_regex_excludes_file_from_patch() {
        // The only diff-added file is `b.rs`; ignoring it removes it from the
        // head report, so `head.hits` misses and no added lines remain to
        // measure — patch coverage becomes undefined rather than 66.7%.
        let (_dir, repo, base) = repo_with_added_file();
        let report = write_head_lcov(&repo);
        let mut cmd = command(report, &base);
        cmd.ignore_filename_regex = vec![r"b\.rs".to_string()];
        let outcome = cmd.run(Some(&repo)).unwrap();
        assert_eq!(outcome.patch_percent, None);
        assert!(!outcome.rendered.contains("`b.rs:2`"));
    }

    #[test]
    fn ignore_filename_regex_drops_file_from_both_reports() {
        // `a.rs` is unchanged but has different coverage in head vs baseline, so
        // `--all-files` would normally surface it in the delta table (it is read
        // from both `head.files` and `baseline.files`). The regex must remove it
        // from both sides symmetrically so it disappears entirely.
        let (_dir, repo, base) = repo_with_added_file();
        let head = format!(
            "SF:{a}\nDA:1,1\nDA:2,1\nDA:3,1\nDA:4,0\nend_of_record\n\
             SF:{b}\nDA:1,1\nDA:2,0\nDA:3,4\nend_of_record\n",
            a = repo.join("a.rs").display(),
            b = repo.join("b.rs").display(),
        );
        let report = repo.join("head.lcov");
        fs::write(&report, head).unwrap();
        let baseline = repo.join("base.lcov");
        fs::write(
            &baseline,
            format!(
                "SF:{}\nDA:1,1\nDA:2,1\nDA:3,0\nDA:4,0\nend_of_record\n",
                repo.join("a.rs").display()
            ),
        )
        .unwrap();

        let mut cmd = command(report, &base);
        cmd.baseline_report = Some(baseline);
        cmd.all_files = true;
        cmd.ignore_filename_regex = vec![r"a\.rs".to_string()];
        let md = cmd.run(Some(&repo)).unwrap().rendered;
        assert!(
            !md.contains("`a.rs`"),
            "ignored a.rs must not appear in the delta table"
        );
        // The un-ignored added file is still reported.
        assert!(md.contains("`b.rs"));
    }

    #[test]
    fn ignore_filename_regex_parses_repeatable_and_comma() {
        use clap::Parser;
        let cmd = DiffCommand::try_parse_from([
            "diff",
            "--report",
            "r.lcov",
            "--ignore-filename-regex",
            "foo,bar",
            "--ignore-filename-regex",
            "baz",
        ])
        .unwrap();
        assert_eq!(cmd.ignore_filename_regex, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn ignore_filename_regex_treats_empty_pattern_as_noop() {
        // A trailing/doubled comma splits to an empty element; an empty regex
        // matches every path and would silently wipe the whole report (and pass
        // a --fail-under-patch gate vacuously). It must be dropped instead.
        let (_dir, repo, base) = repo_with_added_file();
        let report = write_head_lcov(&repo);
        let mut cmd = command(report, &base);
        cmd.ignore_filename_regex = vec![String::new(), r"nomatch\.rs".to_string()];
        let outcome = cmd.run(Some(&repo)).unwrap();
        // b.rs is still measured: 2/3, not wiped to nothing.
        assert_eq!(outcome.patch_percent, Some(2.0 / 3.0 * 100.0));
        assert!(outcome.rendered.contains("`b.rs:2`"));
    }

    #[test]
    fn invalid_ignore_pattern_errors() {
        let (_dir, repo, base) = repo_with_added_file();
        let report = write_head_lcov(&repo);
        let mut cmd = command(report, &base);
        cmd.ignore_filename_regex = vec!["(unclosed".to_string()];
        assert!(cmd.run(Some(&repo)).is_err());
    }

    #[test]
    fn missing_report_errors() {
        let (_dir, repo, base) = repo_with_added_file();
        let cmd = command(repo.join("nope.lcov"), &base);
        assert!(cmd.run(Some(&repo)).is_err());
    }

    #[test]
    fn render_options_use_flags() {
        let (_dir, repo, base) = repo_with_added_file();
        let mut cmd = command(repo.join("head.lcov"), &base);
        cmd.artifact_url = Some("https://artifact".to_string());
        cmd.collapse_ranges = true;
        let opts = cmd.render_options();
        assert_eq!(opts.artifact_url.as_deref(), Some("https://artifact"));
        assert!(opts.collapse_ranges);
    }

    #[tokio::test]
    async fn execute_succeeds_and_gate_bails() {
        let (_dir, repo, base) = repo_with_added_file();
        let report = write_head_lcov(&repo);
        // Passing gate: execute prints and returns Ok.
        let mut cmd = command(report.clone(), &base);
        cmd.fail_under_patch = Some(10.0);
        assert!(cmd.execute(Some(&repo)).await.is_ok());

        // Failing gate: execute returns Err.
        let mut cmd = command(report, &base);
        cmd.fail_under_patch = Some(99.0);
        assert!(cmd.execute(Some(&repo)).await.is_err());
    }

    #[tokio::test]
    async fn execute_folds_deprecated_format_flag() {
        let (_dir, repo, base) = repo_with_added_file();
        let report = write_head_lcov(&repo);
        // The deprecated `--format` is folded into `output` with a warning
        // before `run` reads it.
        let mut cmd = command(report, &base);
        cmd.format = Some(OutputFormatArg::Yaml);
        assert!(cmd.execute(Some(&repo)).await.is_ok());
    }

    /// The injected repo root drives BOTH the git repository and relative
    /// report-path resolution. With a RELATIVE `--report` and the injected repo
    /// at `repo` (never the process CWD), the report must be read from
    /// `repo/head.lcov` — proving `-C` is honored consistently and not split
    /// between the injected repo and the ambient CWD.
    #[test]
    fn run_anchors_repo_and_relative_report_to_injected_root() {
        let (_dir, repo, base) = repo_with_added_file();
        write_head_lcov(&repo); // writes <repo>/head.lcov
        let outcome = command(PathBuf::from("head.lcov"), &base)
            .run(Some(&repo))
            .unwrap();
        assert_eq!(outcome.patch_percent, Some(2.0 / 3.0 * 100.0));
        assert!(outcome.rendered.contains("`b.rs:2`"));
    }
}
