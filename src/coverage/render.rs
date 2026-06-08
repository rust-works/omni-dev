//! Rendering of a [`CoverageDiff`] to markdown, YAML, or JSON.
//!
//! The markdown renderer is a drop-in replacement for `scripts/coverage-comment.sh`
//! — same `## Coverage` header, total line with 🟢/🔴 direction, merge-base→head
//! `Comparing` line, the EPS-filtered per-file before/after/Δ table, and the
//! artifact footer — plus a new `### Patch coverage` section (the headline metric
//! the aggregate comment could never show) and an indirect-changes section.

use anyhow::Result;
use serde::Serialize;

use super::analysis::CoverageDiff;
use crate::data::{FieldDocumentation, FieldExplanation};

/// Minimum per-file change (percentage points) for a row to be listed, matching
/// `scripts/coverage-comment.sh` (suppresses floating-point noise).
const EPS: f64 = 0.05;

/// Output serialisation for `coverage diff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Markdown PR comment (default).
    Markdown,
    /// YAML following the project's structured-output conventions.
    Yaml,
    /// JSON for programmatic use.
    Json,
}

/// Decoration inputs and options for rendering.
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    /// Link to the full coverage-summary artifact.
    pub artifact_url: Option<String>,
    /// Link to the CI run.
    pub run_url: Option<String>,
    /// Base (merge-base) commit SHA.
    pub base_sha: Option<String>,
    /// Head commit SHA.
    pub head_sha: Option<String>,
    /// Commit-URL prefix for linking SHAs (e.g. `https://…/<repo>/commit`).
    pub commit_url: Option<String>,
    /// Collapse consecutive uncovered new lines into ranges (e.g. `9-11`).
    pub collapse_ranges: bool,
}

/// Renders `diff` in the requested `format`.
pub fn render(diff: &CoverageDiff, opts: &RenderOptions, format: OutputFormat) -> Result<String> {
    match format {
        OutputFormat::Markdown => Ok(render_markdown(diff, opts)),
        OutputFormat::Yaml => {
            let mut view = CoverageDiffView::build(diff, opts);
            view.update_field_presence();
            crate::data::yaml::to_yaml(&view)
        }
        OutputFormat::Json => {
            let mut view = CoverageDiffView::build(diff, opts);
            view.update_field_presence();
            Ok(serde_json::to_string_pretty(&view)?)
        }
    }
}

// ---------------------------------------------------------------------------
// Number formatting (mirrors the jq `rnd`/`pct` helpers in coverage-comment.sh)
// ---------------------------------------------------------------------------

/// Rounds to two decimal places, normalising negative zero to `0.0`.
fn round2(x: f64) -> f64 {
    let r = (x * 100.0).round() / 100.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// Formats a number with up to two decimals, trailing zeros trimmed (`100`, `65.4`).
fn fmt_num(x: f64) -> String {
    let s = format!("{:.2}", round2(x));
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Formats an optional percentage; `None` renders as an em dash.
fn pct(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{}%", fmt_num(v)),
        None => "—".to_string(),
    }
}

/// Direction emoji for a percentage-point delta.
fn arrow(d: f64) -> &'static str {
    if d > 0.0 {
        "🟢"
    } else if d < 0.0 {
        "🔴"
    } else {
        "⚪"
    }
}

/// Renders a commit ref as a short, optionally-linked SHA.
fn sha_ref(sha: &str, commit_url: Option<&str>) -> String {
    let short: String = sha.chars().take(7).collect();
    match commit_url {
        Some(url) if !url.is_empty() => format!("[`{short}`]({url}/{sha})"),
        _ => format!("`{short}`"),
    }
}

/// Collapses a sorted, de-duplicated line list into `5, 9-11` style ranges.
fn collapse_ranges(lines: &[u32]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = lines[i];
        let mut end = start;
        while i + 1 < lines.len() && lines[i + 1] == end + 1 {
            end += 1;
            i += 1;
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}-{end}"));
        }
        i += 1;
    }
    parts.join(", ")
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn render_markdown(diff: &CoverageDiff, opts: &RenderOptions) -> String {
    let mut out = String::new();
    out.push_str("## Coverage\n\n");

    // Total line.
    if diff.has_baseline {
        match (diff.total_after, diff.total_before) {
            (after, Some(before)) => {
                let d = after.unwrap_or(0.0) - before;
                out.push_str(&format!(
                    "Total: **{}** {} {} pp vs `main`\n\n",
                    pct(after),
                    arrow(d),
                    fmt_num(d)
                ));
            }
            (after, None) => {
                out.push_str(&format!("Total: **{}**\n\n", pct(after)));
            }
        }
    } else {
        out.push_str(&format!("Total: **{}**\n\n", pct(diff.total_after)));
    }

    // Comparing line.
    if let (Some(base), Some(head)) = (opts.base_sha.as_deref(), opts.head_sha.as_deref()) {
        if !base.is_empty() && !head.is_empty() {
            out.push_str(&format!(
                "Comparing {}..{} _(merge-base → PR head)_\n\n",
                sha_ref(base, opts.commit_url.as_deref()),
                sha_ref(head, opts.commit_url.as_deref())
            ));
        }
    }

    if diff.has_baseline {
        render_delta_table(diff, &mut out);
    } else {
        out.push_str(
            "_No baseline available yet (first run, or the `main` baseline artifact was \
             missing). Per-file deltas will appear on PRs once a baseline has been published \
             from `main`._\n\n",
        );
    }

    render_patch_section(diff, opts, &mut out);

    if diff.has_baseline && !diff.indirect.is_empty() {
        render_indirect_section(diff, &mut out);
    }

    render_footer(opts, &mut out);
    out
}

fn render_delta_table(diff: &CoverageDiff, out: &mut String) {
    // Build rows matching coverage-comment.sh: new files, or |delta| >= EPS.
    struct Row {
        path: String,
        before: Option<f64>,
        after: Option<f64>,
        delta: Option<f64>,
    }
    let mut rows: Vec<Row> = diff
        .file_deltas
        .iter()
        .map(|fd| {
            let delta = fd.before.map(|before| fd.after.unwrap_or(0.0) - before);
            Row {
                path: fd.path.clone(),
                before: fd.before,
                after: fd.after,
                delta,
            }
        })
        .filter(|r| r.delta.is_none() || r.delta.is_some_and(|d| d.abs() >= EPS))
        .collect();
    // New files (no delta) sort to the top, then largest decreases first.
    rows.sort_by(|a, b| {
        a.delta
            .unwrap_or(-1e9)
            .partial_cmp(&b.delta.unwrap_or(-1e9))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if rows.is_empty() {
        out.push_str("_No per-file coverage changes vs `main`._\n\n");
        return;
    }

    out.push_str("| File | Before | After | Δ |\n");
    out.push_str("|------|-------:|------:|---|\n");
    for r in rows {
        let change = match r.delta {
            None => "🆕 new".to_string(),
            Some(d) => format!("{} {} pp", arrow(d), fmt_num(d)),
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            r.path,
            pct(r.before),
            pct(r.after),
            change
        ));
    }
    out.push('\n');
}

fn render_patch_section(diff: &CoverageDiff, opts: &RenderOptions, out: &mut String) {
    out.push_str("### Patch coverage\n\n");

    if diff.patch.total() == 0 {
        out.push_str("_No new executable lines added by this diff._\n\n");
        return;
    }

    out.push_str(&format!(
        "Patch: **{}** ({}/{} new lines covered)\n\n",
        pct(diff.patch.percent()),
        diff.patch.covered,
        diff.patch.total()
    ));

    if !diff.file_patches.is_empty() {
        out.push_str("| File | Patch | Uncovered new lines |\n");
        out.push_str("|------|------:|---------------------|\n");
        for fp in &diff.file_patches {
            let uncovered = if fp.uncovered_lines.is_empty() {
                "—".to_string()
            } else if opts.collapse_ranges {
                collapse_ranges(&fp.uncovered_lines)
            } else {
                fp.uncovered_lines
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push_str(&format!(
                "| `{}` | {} ({}/{}) | {} |\n",
                fp.path,
                pct(fp.patch.percent()),
                fp.patch.covered,
                fp.patch.total(),
                uncovered
            ));
        }
        out.push('\n');
    }

    if !diff.uncovered_new_lines.is_empty() {
        out.push_str(&format!(
            "<details><summary>Uncovered new lines ({})</summary>\n\n",
            diff.uncovered_new_lines.len()
        ));
        for (path, line) in &diff.uncovered_new_lines {
            out.push_str(&format!("- `{path}:{line}`\n"));
        }
        out.push_str("\n</details>\n\n");
    }
}

fn render_indirect_section(diff: &CoverageDiff, out: &mut String) {
    out.push_str("### Indirect coverage changes\n\n");
    out.push_str(&format!(
        "🔴 {} lines lost coverage, 🟢 {} lines gained coverage on unchanged code.\n\n",
        diff.indirect_newly_uncovered(),
        diff.indirect_newly_covered()
    ));
    out.push_str("<details><summary>Indirect changes</summary>\n\n");
    for change in &diff.indirect {
        let transition = if change.became_covered {
            "🟢 uncovered → covered"
        } else {
            "🔴 covered → uncovered"
        };
        out.push_str(&format!(
            "- `{}:{}` {}\n",
            change.path, change.head_line, transition
        ));
    }
    out.push_str("\n</details>\n\n");
}

fn render_footer(opts: &RenderOptions, out: &mut String) {
    match opts.artifact_url.as_deref().filter(|u| !u.is_empty()) {
        Some(artifact) => {
            out.push_str(&format!(
                "<sub>📦 [Full per-file coverage summary]({artifact})"
            ));
            if let Some(run) = opts.run_url.as_deref().filter(|u| !u.is_empty()) {
                out.push_str(&format!(" · [run summary]({run})"));
            }
            out.push_str("</sub>\n");
        }
        None => {
            out.push_str(
                "<sub>Full per-file summary is attached as the **coverage-summary** build \
                 artifact.</sub>\n",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Structured (YAML / JSON) view
// ---------------------------------------------------------------------------

/// Serializable view of a [`CoverageDiff`] for YAML/JSON output, carrying the
/// field-presence explanation block the project uses for structured output.
#[derive(Debug, Clone, Serialize)]
struct CoverageDiffView {
    explanation: FieldExplanation,
    patch_coverage: PatchView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uncovered_new_lines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_delta: Option<ProjectDeltaView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    indirect_changes: Option<IndirectView>,
}

#[derive(Debug, Clone, Serialize)]
struct PatchView {
    percent: Option<f64>,
    covered: u64,
    total: u64,
    files: Vec<FilePatchView>,
}

#[derive(Debug, Clone, Serialize)]
struct FilePatchView {
    path: String,
    percent: Option<f64>,
    covered: u64,
    total: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uncovered_lines: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectDeltaView {
    total_before: Option<f64>,
    total_after: Option<f64>,
    files: Vec<FileDeltaView>,
}

#[derive(Debug, Clone, Serialize)]
struct FileDeltaView {
    path: String,
    before: Option<f64>,
    after: Option<f64>,
    delta: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct IndirectView {
    newly_covered: usize,
    newly_uncovered: usize,
    lines: Vec<IndirectLineView>,
}

#[derive(Debug, Clone, Serialize)]
struct IndirectLineView {
    path: String,
    head_line: u32,
    base_line: u32,
    transition: String,
}

impl CoverageDiffView {
    fn build(diff: &CoverageDiff, _opts: &RenderOptions) -> Self {
        let patch_coverage = PatchView {
            percent: diff.patch.percent().map(round2),
            covered: diff.patch.covered,
            total: diff.patch.total(),
            files: diff
                .file_patches
                .iter()
                .map(|fp| FilePatchView {
                    path: fp.path.clone(),
                    percent: fp.patch.percent().map(round2),
                    covered: fp.patch.covered,
                    total: fp.patch.total(),
                    uncovered_lines: fp.uncovered_lines.clone(),
                })
                .collect(),
        };

        let uncovered_new_lines = diff
            .uncovered_new_lines
            .iter()
            .map(|(path, line)| format!("{path}:{line}"))
            .collect();

        let (project_delta, indirect_changes) = if diff.has_baseline {
            let project_delta = ProjectDeltaView {
                total_before: diff.total_before.map(round2),
                total_after: diff.total_after.map(round2),
                files: diff
                    .file_deltas
                    .iter()
                    .map(|fd| FileDeltaView {
                        path: fd.path.clone(),
                        before: fd.before.map(round2),
                        after: fd.after.map(round2),
                        delta: fd.delta().map(round2),
                    })
                    .collect(),
            };
            let indirect_changes = IndirectView {
                newly_covered: diff.indirect_newly_covered(),
                newly_uncovered: diff.indirect_newly_uncovered(),
                lines: diff
                    .indirect
                    .iter()
                    .map(|c| IndirectLineView {
                        path: c.path.clone(),
                        head_line: c.head_line,
                        base_line: c.base_line,
                        transition: if c.became_covered {
                            "uncovered_to_covered".to_string()
                        } else {
                            "covered_to_uncovered".to_string()
                        },
                    })
                    .collect(),
            };
            (Some(project_delta), Some(indirect_changes))
        } else {
            (None, None)
        };

        Self {
            explanation: explanation(),
            patch_coverage,
            uncovered_new_lines,
            project_delta,
            indirect_changes,
        }
    }

    /// Sets the `present` flag on each documented field based on the data.
    fn update_field_presence(&mut self) {
        let has_patch_files = !self.patch_coverage.files.is_empty();
        let has_uncovered = !self.uncovered_new_lines.is_empty();
        let has_baseline = self.project_delta.is_some();
        let has_indirect = self
            .indirect_changes
            .as_ref()
            .is_some_and(|i| !i.lines.is_empty());
        for field in &mut self.explanation.fields {
            field.present = match field.name.as_str() {
                "patch_coverage.percent" | "patch_coverage.covered" | "patch_coverage.total" => {
                    true
                }
                "patch_coverage.files[].path" => has_patch_files,
                "uncovered_new_lines[]" => has_uncovered,
                "project_delta.total_after" | "project_delta.files[].path" => has_baseline,
                "indirect_changes.lines[].path" => has_indirect,
                _ => false,
            };
        }
    }
}

/// Builds the static field-explanation block for the coverage view.
fn explanation() -> FieldExplanation {
    fn field(name: &str, text: &str) -> FieldDocumentation {
        FieldDocumentation {
            name: name.to_string(),
            text: text.to_string(),
            command: None,
            present: false,
        }
    }
    FieldExplanation {
        text: "Diff/patch coverage analysis. `patch_coverage` attributes coverage to the lines \
               this diff added (needs only the head report + diff). `project_delta` and \
               `indirect_changes` are present only when a baseline report was supplied."
            .to_string(),
        fields: vec![
            field(
                "patch_coverage.percent",
                "Percentage of added, instrumented lines that are covered.",
            ),
            field("patch_coverage.covered", "Count of covered added lines."),
            field(
                "patch_coverage.total",
                "Count of added, instrumented lines (the patch-coverage denominator).",
            ),
            field(
                "patch_coverage.files[].path",
                "Per-file patch coverage for files that added instrumented lines.",
            ),
            field(
                "uncovered_new_lines[]",
                "Actionable `file:line` list of added lines that are not covered.",
            ),
            field(
                "project_delta.total_after",
                "Project line coverage before/after; present only with a baseline report.",
            ),
            field(
                "project_delta.files[].path",
                "Per-file before/after coverage and delta; present only with a baseline report.",
            ),
            field(
                "indirect_changes.lines[].path",
                "Lines whose coverage flipped without their content changing; needs a baseline.",
            ),
        ],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::coverage::analysis::{FileDelta, FilePatch, IndirectChange, PatchCoverage};

    #[test]
    fn fmt_num_trims_trailing_zeros() {
        assert_eq!(fmt_num(100.0), "100");
        assert_eq!(fmt_num(65.4), "65.4");
        assert_eq!(fmt_num(65.432), "65.43");
        assert_eq!(fmt_num(50.0), "50");
        assert_eq!(fmt_num(-0.001), "0");
    }

    #[test]
    fn collapse_ranges_groups_consecutive() {
        assert_eq!(collapse_ranges(&[5]), "5");
        assert_eq!(collapse_ranges(&[9, 10, 11]), "9-11");
        assert_eq!(collapse_ranges(&[5, 9, 10, 11, 20]), "5, 9-11, 20");
    }

    #[test]
    fn sha_ref_links_when_url_present() {
        assert_eq!(sha_ref("abcdef1234", None), "`abcdef1`");
        assert_eq!(
            sha_ref("abcdef1234", Some("https://x/commit")),
            "[`abcdef1`](https://x/commit/abcdef1234)"
        );
    }

    fn sample_diff() -> CoverageDiff {
        CoverageDiff {
            patch: PatchCoverage {
                covered: 4,
                uncovered: 1,
            },
            file_patches: vec![FilePatch {
                path: "src/a.rs".to_string(),
                patch: PatchCoverage {
                    covered: 4,
                    uncovered: 1,
                },
                uncovered_lines: vec![9],
            }],
            uncovered_new_lines: vec![("src/a.rs".to_string(), 9)],
            total_after: Some(80.0),
            ..Default::default()
        }
    }

    #[test]
    fn markdown_without_baseline_has_patch_section() {
        let diff = sample_diff();
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("## Coverage"));
        assert!(md.contains("Total: **80%**"));
        assert!(md.contains("### Patch coverage"));
        assert!(md.contains("Patch: **80%** (4/5 new lines covered)"));
        assert!(md.contains("`src/a.rs:9`"));
        assert!(md.contains("No baseline available yet"));
    }

    #[test]
    fn markdown_with_baseline_shows_total_delta_and_indirect() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(75.0);
        diff.indirect = vec![IndirectChange {
            path: "src/b.rs".to_string(),
            base_line: 5,
            head_line: 5,
            became_covered: false,
        }];
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("🟢 5 pp vs `main`"));
        assert!(md.contains("### Indirect coverage changes"));
        assert!(md.contains("`src/b.rs:5`"));
    }

    #[test]
    fn json_round_trips() {
        let diff = sample_diff();
        let json = render(&diff, &RenderOptions::default(), OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["patch_coverage"]["covered"], 4);
        assert_eq!(value["patch_coverage"]["total"], 5);
        assert_eq!(value["uncovered_new_lines"][0], "src/a.rs:9");
        // Baseline-only sections absent without a baseline.
        assert!(value.get("project_delta").is_none());
    }

    #[test]
    fn yaml_renders() {
        let diff = sample_diff();
        let yaml = render(&diff, &RenderOptions::default(), OutputFormat::Yaml).unwrap();
        assert!(yaml.contains("patch_coverage:"));
        assert!(yaml.contains("explanation:"));
    }

    /// A baseline diff exercising the delta table (new file, decrease, increase,
    /// below-EPS filtering, an em-dash `After`), the patch table with range
    /// collapsing, and the artifact footer.
    fn baseline_diff() -> CoverageDiff {
        CoverageDiff {
            patch: PatchCoverage {
                covered: 2,
                uncovered: 4,
            },
            file_patches: vec![FilePatch {
                path: "src/a.rs".to_string(),
                patch: PatchCoverage {
                    covered: 2,
                    uncovered: 4,
                },
                uncovered_lines: vec![9, 10, 11, 15],
            }],
            uncovered_new_lines: vec![
                ("src/a.rs".to_string(), 9),
                ("src/a.rs".to_string(), 10),
                ("src/a.rs".to_string(), 11),
                ("src/a.rs".to_string(), 15),
            ],
            has_baseline: true,
            total_after: Some(80.0),
            total_before: Some(80.0), // equal → ⚪ 0 pp
            file_deltas: vec![
                FileDelta {
                    path: "src/new.rs".to_string(),
                    before: None,
                    after: Some(50.0),
                },
                FileDelta {
                    path: "src/down.rs".to_string(),
                    before: Some(100.0),
                    after: Some(70.0),
                },
                FileDelta {
                    path: "src/up.rs".to_string(),
                    before: Some(70.0),
                    after: Some(90.0),
                },
                FileDelta {
                    path: "src/tiny.rs".to_string(),
                    before: Some(90.0),
                    after: Some(90.02), // below EPS → filtered out
                },
                FileDelta {
                    path: "src/gone.rs".to_string(),
                    before: Some(50.0),
                    after: None, // After renders as em dash
                },
            ],
            indirect: Vec::new(),
        }
    }

    #[test]
    fn markdown_delta_table_and_footer() {
        let diff = baseline_diff();
        let opts = RenderOptions {
            artifact_url: Some("https://artifact".to_string()),
            run_url: Some("https://run".to_string()),
            collapse_ranges: true,
            ..Default::default()
        };
        let md = render(&diff, &opts, OutputFormat::Markdown).unwrap();
        assert!(md.contains("⚪ 0 pp vs `main`"));
        assert!(md.contains("| `src/new.rs` | — | 50% | 🆕 new |"));
        assert!(md.contains("🔴 -30 pp"));
        assert!(md.contains("🟢 20 pp"));
        assert!(md.contains("| `src/gone.rs` | 50% | — | 🔴 -50 pp |"));
        assert!(!md.contains("tiny.rs"), "below-EPS row must be filtered");
        // Patch table with collapsed ranges.
        assert!(md.contains("9-11, 15"));
        // Artifact footer with run link.
        assert!(md.contains("[Full per-file coverage summary](https://artifact)"));
        assert!(md.contains("[run summary](https://run)"));
    }

    #[test]
    fn markdown_comparing_line_and_covered_indirect() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(80.0);
        diff.indirect = vec![IndirectChange {
            path: "src/b.rs".to_string(),
            base_line: 5,
            head_line: 5,
            became_covered: true,
        }];
        let opts = RenderOptions {
            base_sha: Some("abcdef123".to_string()),
            head_sha: Some("fedcba321".to_string()),
            commit_url: Some("https://x/commit".to_string()),
            ..Default::default()
        };
        let md = render(&diff, &opts, OutputFormat::Markdown).unwrap();
        assert!(md.contains("Comparing [`abcdef1`](https://x/commit/abcdef123)"));
        assert!(md.contains("🟢 uncovered → covered"));
    }

    #[test]
    fn markdown_no_per_file_changes() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(80.0);
        // No file_deltas → "no per-file coverage changes".
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("_No per-file coverage changes vs `main`._"));
    }

    #[test]
    fn markdown_baseline_without_total_before() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = None;
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("Total: **80%**"));
        assert!(!md.contains("pp vs"));
    }

    #[test]
    fn markdown_no_added_lines() {
        let diff = CoverageDiff {
            total_after: Some(50.0),
            ..Default::default()
        };
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("_No new executable lines added by this diff._"));
    }

    #[test]
    fn json_and_yaml_with_baseline_include_project_delta() {
        let diff = baseline_diff();
        let json = render(&diff, &RenderOptions::default(), OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("project_delta").is_some());
        assert_eq!(value["project_delta"]["total_after"], 80.0);
        assert!(value.get("indirect_changes").is_some());

        let yaml = render(&diff, &RenderOptions::default(), OutputFormat::Yaml).unwrap();
        assert!(yaml.contains("project_delta:"));
    }
}
