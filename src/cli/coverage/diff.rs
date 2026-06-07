//! `omni-dev coverage diff` — diff/patch coverage analysis.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use git2::Repository;

use crate::coverage::{
    analyze, default_base_ref, parse, render, DiffModel, Format, OutputFormat, RenderOptions,
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
    #[arg(long, value_enum, default_value_t = OutputFormatArg::Markdown)]
    pub format: OutputFormatArg,

    /// Fail (non-zero exit) when patch coverage is below this percentage.
    #[arg(long, value_name = "PCT")]
    pub fail_under_patch: Option<f64>,

    /// Repository path.
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub repo: PathBuf,

    /// Override the path prefix stripped from report file paths to make them
    /// repo-relative (default: the repository working directory).
    #[arg(long, value_name = "PATH")]
    pub strip_prefix: Option<PathBuf>,

    /// Collapse consecutive uncovered new lines into ranges (e.g. `9-11`).
    #[arg(long)]
    pub collapse_ranges: bool,

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
    pub async fn execute(self) -> Result<()> {
        let outcome = self.run()?;
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
    pub fn run(&self) -> Result<DiffOutcome> {
        let repo = Repository::open(&self.repo)
            .with_context(|| format!("could not open git repository at {}", self.repo.display()))?;

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

        let head = self.load_report(&self.report, self.report_format, strip_prefix.as_deref())?;
        let baseline = match &self.baseline_report {
            Some(path) => Some(self.load_report(
                path,
                self.baseline_report_format,
                strip_prefix.as_deref(),
            )?),
            None => None,
        };

        let diff = DiffModel::between(&repo, &base_ref, self.head_ref.as_deref())?;
        let result = analyze(&head, &diff, baseline.as_ref());

        let opts = self.render_options();
        let rendered = render(&result, &opts, self.format.into())?;

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
    fn load_report(
        &self,
        path: &std::path::Path,
        format: ReportFormat,
        strip_prefix: Option<&std::path::Path>,
    ) -> Result<crate::coverage::CoverageReport> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("could not read coverage report {}", path.display()))?;
        let mut report = parse(&content, format.into_format())
            .with_context(|| format!("could not parse coverage report {}", path.display()))?;
        if let Some(prefix) = strip_prefix {
            report.strip_prefix(prefix);
        }
        Ok(report)
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
