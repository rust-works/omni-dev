//! MCP tool handler for diff/patch coverage analysis.
//!
//! Fits the git/PR family: `coverage_diff` reuses the CLI's `DiffCommand::run`
//! (`src/cli/coverage/diff.rs`) — the non-printing core behind
//! `omni-dev coverage diff` — and returns the rendered report plus the patch
//! percentage and gate result as YAML. The head coverage report is a filesystem
//! path the caller must provide.

use std::path::PathBuf;

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::cancel::spawn_blocking_cancellable;
use super::git_tools::build_truncated_result;
use super::server::OmniDevServer;
use crate::cli::coverage::diff::{DiffCommand, DiffOutcome, OutputFormatArg, ReportFormat};

/// Coverage report format (MCP mirror of the CLI's `ReportFormat`).
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageReportFormat {
    /// Auto-detect from report content (default).
    #[default]
    Auto,
    /// lcov trace file.
    Lcov,
    /// llvm-cov JSON export (`cargo llvm-cov report --json`).
    LlvmCovJson,
    /// Cobertura XML.
    Cobertura,
}

impl From<CoverageReportFormat> for ReportFormat {
    fn from(value: CoverageReportFormat) -> Self {
        match value {
            CoverageReportFormat::Auto => Self::Auto,
            CoverageReportFormat::Lcov => Self::Lcov,
            CoverageReportFormat::LlvmCovJson => Self::LlvmCovJson,
            CoverageReportFormat::Cobertura => Self::Cobertura,
        }
    }
}

/// Rendered output format (MCP mirror of the CLI's `OutputFormatArg`).
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CoverageOutputFormat {
    /// Markdown PR comment (default).
    #[default]
    Markdown,
    /// YAML structured output.
    Yaml,
    /// JSON output.
    Json,
}

impl From<CoverageOutputFormat> for OutputFormatArg {
    fn from(value: CoverageOutputFormat) -> Self {
        match value {
            CoverageOutputFormat::Markdown => Self::Markdown,
            CoverageOutputFormat::Yaml => Self::Yaml,
            CoverageOutputFormat::Json => Self::Json,
        }
    }
}

/// Parameters for the `coverage_diff` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CoverageDiffParams {
    /// Head coverage report path (lcov / llvm-cov-json / cobertura). Required.
    pub report: String,
    /// Format of `report` (auto-detected by default).
    #[serde(default)]
    pub report_format: CoverageReportFormat,
    /// Base revision to diff against (default: merge-base of `origin/main` and
    /// `HEAD`).
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Head revision the report was measured at (default: `HEAD`).
    #[serde(default)]
    pub head_ref: Option<String>,
    /// Optional baseline coverage report path; enables project deltas and
    /// indirect-change detection.
    #[serde(default)]
    pub baseline_report: Option<String>,
    /// Format of `baseline_report` (auto-detected by default).
    #[serde(default)]
    pub baseline_report_format: CoverageReportFormat,
    /// Rendered output format. Defaults to `markdown`.
    #[serde(default)]
    pub format: CoverageOutputFormat,
    /// Report a below-gate result when patch coverage is below this percentage
    /// (the tool never fails the call; it reports `below_gate` instead).
    #[serde(default)]
    pub fail_under_patch: Option<f64>,
    /// Override the path prefix stripped from report paths to make them
    /// repo-relative (default: the repository working directory).
    #[serde(default)]
    pub strip_prefix: Option<String>,
    /// Exclude files whose repo-relative path matches any of these regexes from
    /// both the head and baseline reports before computing the diff. Matching is
    /// unanchored, applied after `strip_prefix` (same semantics as
    /// `cargo llvm-cov --ignore-filename-regex`).
    #[serde(default)]
    pub ignore_filename_regex: Vec<String>,
    /// Collapse consecutive uncovered new lines into ranges (e.g. `9-11`).
    #[serde(default)]
    pub collapse_ranges: bool,
    /// Report per-file deltas and indirect changes for ALL files, not just the
    /// ones the diff touches.
    #[serde(default)]
    pub all_files: bool,
    /// Link to the full coverage-summary artifact (markdown footer).
    #[serde(default)]
    pub artifact_url: Option<String>,
    /// Link to the CI run (markdown footer).
    #[serde(default)]
    pub run_url: Option<String>,
    /// Base (merge-base) commit SHA shown in the markdown `Comparing` line.
    #[serde(default)]
    pub base_sha: Option<String>,
    /// Head commit SHA shown in the markdown `Comparing` line.
    #[serde(default)]
    pub head_sha: Option<String>,
    /// Commit-URL prefix for linking SHAs.
    #[serde(default)]
    pub commit_url: Option<String>,
    /// Path to the git repository. Defaults to the current working directory.
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[allow(missing_docs)] // #[tool_router] generates a pub `coverage_tool_router` fn.
#[tool_router(router = coverage_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: diff/patch coverage analysis.
    #[tool(
        description = "Analyze diff/patch coverage from a per-line coverage report plus the git \
                       diff, and return the rendered report with the patch-coverage percentage \
                       and gate result as YAML. Read-only. Mirrors `omni-dev coverage diff`. \
                       `report` is a required filesystem path to the head coverage report \
                       (lcov / llvm-cov-json / cobertura, auto-detected). `format` renders the \
                       report as `markdown` (default), `yaml`, or `json`. Unlike the CLI this \
                       tool never fails the call on a low `fail_under_patch`; it reports \
                       `below_gate: true` instead."
    )]
    pub async fn coverage_diff(
        &self,
        Parameters(params): Parameters<CoverageDiffParams>,
        cancellation: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let repo_path = params.repo_path.clone().map(PathBuf::from);
        let cmd = DiffCommand {
            report: PathBuf::from(params.report),
            report_format: params.report_format.into(),
            base_ref: params.base_ref,
            head_ref: params.head_ref,
            baseline_report: params.baseline_report.map(PathBuf::from),
            baseline_report_format: params.baseline_report_format.into(),
            // `output` is the real selector (post-#1206); `format` is the
            // hidden deprecated CLI alias, unused here.
            output: params.format.into(),
            format: None,
            fail_under_patch: params.fail_under_patch,
            strip_prefix: params.strip_prefix.map(PathBuf::from),
            ignore_filename_regex: params.ignore_filename_regex,
            collapse_ranges: params.collapse_ranges,
            all_files: params.all_files,
            artifact_url: params.artifact_url,
            run_url: params.run_url,
            base_sha: params.base_sha,
            head_sha: params.head_sha,
            commit_url: params.commit_url,
        };

        let payload = spawn_blocking_cancellable(&cancellation, move || {
            let outcome = cmd.run(repo_path.as_deref())?;
            Ok(format_coverage_payload(&outcome))
        })
        .await?;

        Ok(build_truncated_result(payload))
    }
}

/// Formats the [`DiffOutcome`] as a YAML payload: the patch percentage, the
/// gate result, and the rendered report as a block scalar.
fn format_coverage_payload(outcome: &DiffOutcome) -> String {
    let patch = outcome
        .patch_percent
        .map_or_else(|| "null".to_string(), |p| format!("{p:.4}"));
    let rendered = outcome
        .rendered
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# coverage_diff outcome\npatch_percent: {patch}\nbelow_gate: {}\nrendered: |\n{rendered}",
        outcome.below_gate,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn params_require_report() {
        assert!(serde_json::from_str::<CoverageDiffParams>("{}").is_err());
        let p: CoverageDiffParams = serde_json::from_str(r#"{"report": "cov.info"}"#).unwrap();
        assert_eq!(p.report, "cov.info");
        assert!(matches!(p.report_format, CoverageReportFormat::Auto));
        assert!(matches!(p.format, CoverageOutputFormat::Markdown));
    }

    #[test]
    fn report_format_parses_kebab_case() {
        let p: CoverageDiffParams =
            serde_json::from_str(r#"{"report": "c", "report_format": "llvm-cov-json"}"#).unwrap();
        assert!(matches!(p.report_format, CoverageReportFormat::LlvmCovJson));
    }

    #[test]
    fn format_payload_wraps_rendered_as_block_scalar() {
        let outcome = DiffOutcome {
            rendered: "line1\nline2".to_string(),
            patch_percent: Some(87.5),
            below_gate: false,
        };
        let payload = format_coverage_payload(&outcome);
        assert!(payload.contains("patch_percent: 87.5000"));
        assert!(payload.contains("below_gate: false"));
        assert!(payload.contains("  line1"));
        assert!(payload.contains("  line2"));
    }

    #[test]
    fn format_payload_renders_null_patch_percent() {
        let outcome = DiffOutcome {
            rendered: "x".to_string(),
            patch_percent: None,
            below_gate: true,
        };
        let payload = format_coverage_payload(&outcome);
        assert!(payload.contains("patch_percent: null"));
        assert!(payload.contains("below_gate: true"));
    }

    #[test]
    fn report_format_into_covers_all_variants() {
        assert_eq!(
            ReportFormat::from(CoverageReportFormat::Auto),
            ReportFormat::Auto
        );
        assert_eq!(
            ReportFormat::from(CoverageReportFormat::Lcov),
            ReportFormat::Lcov
        );
        assert_eq!(
            ReportFormat::from(CoverageReportFormat::LlvmCovJson),
            ReportFormat::LlvmCovJson
        );
        assert_eq!(
            ReportFormat::from(CoverageReportFormat::Cobertura),
            ReportFormat::Cobertura
        );
    }

    #[test]
    fn output_format_into_covers_all_variants() {
        assert_eq!(
            OutputFormatArg::from(CoverageOutputFormat::Markdown),
            OutputFormatArg::Markdown
        );
        assert_eq!(
            OutputFormatArg::from(CoverageOutputFormat::Yaml),
            OutputFormatArg::Yaml
        );
        assert_eq!(
            OutputFormatArg::from(CoverageOutputFormat::Json),
            OutputFormatArg::Json
        );
    }

    #[tokio::test]
    async fn coverage_diff_handler_bad_repo_returns_tool_error() {
        use crate::mcp::server::OmniDevServer;
        use rmcp::handler::server::wrapper::Parameters;
        use tokio_util::sync::CancellationToken;

        let server = OmniDevServer::new();
        let params = CoverageDiffParams {
            report: "/no/such/report.info".to_string(),
            report_format: CoverageReportFormat::Auto,
            base_ref: None,
            head_ref: None,
            baseline_report: None,
            baseline_report_format: CoverageReportFormat::Auto,
            format: CoverageOutputFormat::Markdown,
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
            repo_path: Some("/no/such/repo/for/mcp/test".to_string()),
        };
        let err = server
            .coverage_diff(Parameters(params), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }
}
