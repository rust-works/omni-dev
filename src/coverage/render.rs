//! Rendering of a [`CoverageDiff`] to markdown, YAML, or JSON.
//!
//! The markdown renderer reproduces the PR comment that the retired
//! `scripts/coverage-comment.sh` shell renderer produced — same `## Coverage`
//! header, total line with 🟢/🔴 direction, merge-base→head `Comparing` line, the
//! EPS-filtered per-file before/after/Δ table, and the artifact footer — plus a
//! `### Patch coverage` section (the headline metric the aggregate comment could
//! never show) and an indirect-changes section. CI renders this comment via
//! `omni-dev coverage diff --format markdown` (see `.github/workflows/ci.yml`).

use anyhow::Result;
use serde::Serialize;

use super::analysis::CoverageDiff;
use crate::data::{FieldDocumentation, FieldExplanation};

/// Minimum per-file change (percentage points) for a row to be listed, matching
/// the original coverage comment (suppresses floating-point noise).
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
// Number formatting (mirrors the jq `rnd`/`pct` helpers of the original comment)
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
///
/// The direction is taken from the *displayed* (rounded) value, not the raw
/// one: every caller prints [`fmt_num`] beside this emoji, and `fmt_num` rounds
/// through [`round2`]. Deriving the arrow from the raw delta let a move in
/// `(-0.005, 0)` render as a red arrow next to `0 pp` — an alarm colour beside a
/// number that says nothing moved.
fn arrow(d: f64) -> &'static str {
    let d = round2(d);
    if d > 0.0 {
        "🟢"
    } else if d < 0.0 {
        "🔴"
    } else {
        "⚪"
    }
}

/// Direction emoji for the *headline* total delta.
///
/// The per-file sections have suppressed cross-run measurement variance since
/// #973 — delta-table rows need `|d| >= EPS`, and an untouched file needs a net
/// move of `NOTABLE_UNCHANGED_LINES` covered lines — but the headline had no
/// equivalent gate, so every flip those sections hid still accumulated here and
/// was painted red. Applying the same `EPS` tolerance keeps the comment
/// internally consistent: a sub-tolerance total move is neutral, while the
/// number itself is still printed truthfully beside it.
fn headline_arrow(d: f64) -> &'static str {
    if d.abs() < EPS {
        "⚪"
    } else {
        arrow(d)
    }
}

/// Annotates the headline when the total moved but no per-file section can
/// account for it — the move is then, by construction, not attributable to this
/// diff. Without this a reader sees a total delta above a comment whose every
/// other section says nothing changed.
fn unattributed_note(diff: &CoverageDiff, d: f64) -> &'static str {
    let moved = round2(d) != 0.0;
    let explained = diff
        .file_deltas
        .iter()
        .any(|fd| fd.delta().is_none_or(|d| d.abs() >= EPS))
        || !diff.notable_unchanged.is_empty();
    if moved && !explained {
        " _(not attributable to this diff)_"
    } else {
        ""
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
                // The percentage displayed is always the real measured value;
                // the movement is computed from the `tolerate`-masked one, so a
                // silenced flip cannot move the arrow or the number beside it.
                let effective = diff.total_after_effective.or(after);
                let d = effective.unwrap_or(0.0) - before;
                out.push_str(&format!(
                    "Total: **{}** {} {} pp vs `main`{}\n\n",
                    pct(after),
                    headline_arrow(d),
                    fmt_num(d),
                    unattributed_note(diff, d)
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
        render_notable_unchanged(diff, &mut out);
    } else {
        out.push_str(
            "_No baseline available yet (first run, or the `main` baseline artifact was \
             missing). Per-file deltas will appear on PRs once a baseline has been published \
             from `main`._\n\n",
        );
    }
    // Also without a baseline: `ignore` still shapes the total and the patch.
    render_markers(diff, &mut out);

    render_patch_section(diff, opts, &mut out);

    if diff.has_baseline && !diff.indirect.is_empty() {
        render_indirect_section(diff, &mut out);
    }

    render_footer(opts, &mut out);
    out
}

fn render_delta_table(diff: &CoverageDiff, out: &mut String) {
    // Build rows as the original comment did: new files, or |delta| >= EPS.
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
            // `delta()` reads the `tolerate`-masked coverage, so a silenced flip
            // does not produce a row; `after` stays the real displayed value.
            let delta = fd.delta();
            Row {
                path: fd.path.clone(),
                before: fd.before,
                after: fd.after,
                delta,
            }
        })
        .filter(|r| r.delta.is_none_or(|d| d.abs() >= EPS))
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

/// Renders the magnitude-gated note for unchanged files whose coverage moved
/// substantially — flagged as *not* attributable to the PR (measurement variance
/// or a cross-file effect like a removed test), kept collapsed so it does not
/// crowd out the actionable sections.
fn render_notable_unchanged(diff: &CoverageDiff, out: &mut String) {
    if diff.notable_unchanged.is_empty() {
        return;
    }
    out.push_str(&format!(
        "<details><summary>ℹ️ {} unchanged file(s) also moved (not attributed to this PR)</summary>\n\n",
        diff.notable_unchanged.len()
    ));
    out.push_str(
        "These files were not modified by this diff; the shift is either measurement variance \
         between the two runs or a cross-file effect (e.g. a removed test).\n\n",
    );
    out.push_str("| File | Before | After | Δ |\n");
    out.push_str("|------|-------:|------:|---|\n");
    for fd in &diff.notable_unchanged {
        let change = match fd.delta() {
            None => "🆕 new".to_string(),
            Some(d) => format!("{} {} pp", arrow(d), fmt_num(d)),
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            fd.path,
            pct(fd.before),
            pct(fd.after),
            change
        ));
    }
    out.push_str("\n</details>\n\n");
}

/// Renders the collapsed note listing every source-marker region that applied.
///
/// Silencing is never invisible: a reviewer can always see which regions were
/// ignored or tolerated, where they are, and why their author silenced them.
fn render_markers(diff: &CoverageDiff, out: &mut String) {
    if diff.markers.is_empty() {
        return;
    }
    let count =
        |kind: crate::coverage::MarkerKind| diff.markers.iter().filter(|m| m.kind == kind).count();
    out.push_str(&format!(
        "<details><summary>🔇 {} ignored region(s), {} tolerated region(s)</summary>\n\n",
        count(crate::coverage::MarkerKind::Ignore),
        count(crate::coverage::MarkerKind::Tolerate)
    ));
    out.push_str(
        "`ignore` removes the lines from both reports; `tolerate` keeps them in the reported \
         percentage but scores them against the baseline, so a cross-run flip cannot move a \
         delta. Regions are read from each revision's own source.\n\n",
    );
    out.push_str("| File | Kind | Lines | Rev | Reason |\n");
    out.push_str("|------|------|-------|-----|--------|\n");
    for marker in &diff.markers {
        let lines = if marker.start == marker.end {
            marker.start.to_string()
        } else {
            format!("{}-{}", marker.start, marker.end)
        };
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {} | {} |\n",
            marker.path,
            marker.kind.as_str(),
            lines,
            marker.side.as_str(),
            marker.reason
        ));
    }
    out.push_str("\n</details>\n\n");
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
    /// Source-marker regions that applied. Empty when no marker was found.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    markers: Vec<MarkerView>,
}

#[derive(Debug, Clone, Serialize)]
struct MarkerView {
    path: String,
    kind: String,
    /// Which revision the region was observed on: `both`, `head`, or `base`.
    side: String,
    start: u32,
    end: u32,
    reason: String,
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
    /// The real, measured head coverage.
    total_after: Option<f64>,
    /// Head coverage with `tolerate` masking applied — the value the reported
    /// deltas are computed from. Present only when masking changed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    total_after_effective: Option<f64>,
    files: Vec<FileDeltaView>,
    /// Unchanged files (not touched by the diff) that nonetheless moved
    /// substantially — flagged as not attributable to the PR.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notable_unchanged: Vec<FileDeltaView>,
}

#[derive(Debug, Clone, Serialize)]
struct FileDeltaView {
    path: String,
    before: Option<f64>,
    /// The real, measured head coverage.
    after: Option<f64>,
    /// Head coverage with `tolerate` masking applied. Present only when masking
    /// changed it, in which case `delta` is `after_effective - before` rather
    /// than `after - before`.
    #[serde(skip_serializing_if = "Option::is_none")]
    after_effective: Option<f64>,
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
            let file_delta_view = |fd: &crate::coverage::analysis::FileDelta| FileDeltaView {
                path: fd.path.clone(),
                before: fd.before.map(round2),
                after: fd.after.map(round2),
                after_effective: fd
                    .is_masked()
                    .then(|| fd.after_effective.map(round2))
                    .flatten(),
                delta: fd.delta().map(round2),
            };
            let project_delta = ProjectDeltaView {
                total_before: diff.total_before.map(round2),
                total_after: diff.total_after.map(round2),
                total_after_effective: (diff.total_after_effective != diff.total_after)
                    .then(|| diff.total_after_effective.map(round2))
                    .flatten(),
                files: diff.file_deltas.iter().map(file_delta_view).collect(),
                notable_unchanged: diff.notable_unchanged.iter().map(file_delta_view).collect(),
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

        let markers = diff
            .markers
            .iter()
            .map(|m| MarkerView {
                path: m.path.clone(),
                kind: m.kind.as_str().to_string(),
                side: m.side.as_str().to_string(),
                start: m.start,
                end: m.end,
                reason: m.reason.clone(),
            })
            .collect();

        Self {
            explanation: explanation(),
            patch_coverage,
            uncovered_new_lines,
            project_delta,
            indirect_changes,
            markers,
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
        let has_markers = !self.markers.is_empty();
        for field in &mut self.explanation.fields {
            field.present = match field.name.as_str() {
                "patch_coverage.percent" | "patch_coverage.covered" | "patch_coverage.total" => {
                    true
                }
                "patch_coverage.files[].path" => has_patch_files,
                "uncovered_new_lines[]" => has_uncovered,
                "project_delta.total_after" | "project_delta.files[].path" => has_baseline,
                "indirect_changes.lines[].path" => has_indirect,
                "markers[].path" => has_markers,
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
            field(
                "markers[].path",
                "Source-marker regions that applied. `kind` is `ignore` (lines removed from both \
                 reports) or `tolerate` (lines kept in the percentages, but their coverage flips \
                 masked). Where a region was tolerated, `delta` is computed from \
                 `after_effective`, not from the displayed `after`.",
            ),
        ],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::coverage::analysis::{
        AppliedMarker, FileDelta, FilePatch, IndirectChange, MarkerSide, PatchCoverage,
    };

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

    /// #1591: the arrow must agree with the number printed beside it. A delta
    /// inside the rounding interval prints `0 pp`, so it must be neutral rather
    /// than raising a red alarm next to a number that says nothing moved.
    #[test]
    fn markdown_sub_rounding_delta_is_neutral() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(80.0);
        diff.total_after = Some(79.996);
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("\u{26aa} 0 pp vs `main`"), "{md}");
        assert!(
            !md.contains("\u{1f534}"),
            "sub-rounding move must not paint red: {md}"
        );
    }

    /// The same pairing in the per-file table, which the EPS row filter happens
    /// to protect, and in the notable-unchanged table, which it does not: that
    /// section is gated on *covered lines* (>= 10), so a large file can reach it
    /// with a sub-rounding percentage-point move.
    #[test]
    fn notable_unchanged_sub_rounding_delta_is_neutral() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(80.0);
        diff.notable_unchanged = vec![FileDelta::new(
            "src/big.rs".to_string(),
            Some(90.0),
            Some(89.998),
        )];
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(
            md.contains("| `src/big.rs` | 90% | 90% | \u{26aa} 0 pp |"),
            "{md}"
        );
    }

    /// #1592: the #2444 shape — a docs-only PR whose headline moved because one
    /// CPU-conditional function flipped between two runner CPUs, while every
    /// per-file section of the same comment reported nothing.
    #[test]
    fn markdown_sub_eps_total_move_is_neutral_and_annotated() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(92.8012);
        diff.total_after = Some(92.7924);
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(
            !md.contains("\u{1f534}"),
            "sub-EPS move must not paint red: {md}"
        );
        assert!(md.contains("Total: **92.79%** \u{26aa} -0.01 pp"), "{md}");
        assert!(md.contains("_(not attributable to this diff)_"), "{md}");
    }

    /// Above the tolerance the headline still reports the direction — but with
    /// nothing below it to account for the move, it says so.
    #[test]
    fn markdown_unexplained_total_move_is_annotated() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(85.0);
        diff.total_after = Some(80.0);
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(
            md.contains("\u{1f534} -5 pp vs `main` _(not attributable to this diff)_"),
            "{md}"
        );
    }

    /// A move a per-file row *does* explain is left unannotated.
    #[test]
    fn markdown_explained_total_move_is_not_annotated() {
        let diff = baseline_diff();
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(!md.contains("not attributable"), "{md}");
    }

    /// A notable-unchanged entry also counts as an explanation, even though it
    /// is not attributed to the PR: the reader can see where the move came from.
    #[test]
    fn markdown_notable_unchanged_explains_total_move() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(85.0);
        diff.total_after = Some(80.0);
        diff.notable_unchanged = vec![FileDelta::new(
            "src/big.rs".to_string(),
            Some(90.0),
            Some(60.0),
        )];
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(!md.contains("not attributable to this diff"), "{md}");
    }

    fn tolerated_marker() -> AppliedMarker {
        AppliedMarker {
            path: "src/util/simd/x86.rs".to_string(),
            kind: crate::coverage::MarkerKind::Tolerate,
            side: MarkerSide::Both,
            start: 41,
            end: 52,
            reason: "CPU-gated: the avx512f arm only runs on Zen 4+".to_string(),
        }
    }

    /// #1593, the motivating case: the headline shows the *real* percentage but
    /// takes its movement from the masked one, so the reported number stays
    /// truthful while the silenced flip stops moving the needle.
    #[test]
    fn markdown_headline_uses_effective_coverage_for_the_delta_only() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(92.8012);
        diff.total_after = Some(92.7924);
        diff.total_after_effective = Some(92.8012);
        diff.markers = vec![tolerated_marker()];
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(
            md.contains("Total: **92.79%** \u{26aa} 0 pp vs `main`"),
            "{md}"
        );
        assert!(
            !md.contains("not attributable"),
            "a masked move is not a move: {md}"
        );
    }

    /// Silencing is never invisible.
    #[test]
    fn markdown_lists_every_applied_marker() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(80.0);
        diff.markers = vec![
            tolerated_marker(),
            AppliedMarker {
                path: "src/generated.rs".to_string(),
                kind: crate::coverage::MarkerKind::Ignore,
                side: MarkerSide::Head,
                start: 7,
                end: 7,
                reason: "generated".to_string(),
            },
        ];
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(
            md.contains("\u{1f507} 1 ignored region(s), 1 tolerated region(s)"),
            "{md}"
        );
        assert!(
            md.contains("| `src/util/simd/x86.rs` | `tolerate` | 41-52 | both |"),
            "{md}"
        );
        assert!(
            md.contains("| `src/generated.rs` | `ignore` | 7 | head |"),
            "{md}"
        );
        assert!(md.contains("the avx512f arm only runs on Zen 4+"), "{md}");
    }

    /// `ignore` shapes the total and the patch even with no baseline, so its
    /// note must appear there too.
    #[test]
    fn markdown_lists_markers_without_a_baseline() {
        let mut diff = sample_diff();
        diff.markers = vec![tolerated_marker()];
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("1 tolerated region(s)"), "{md}");
    }

    #[test]
    fn markdown_omits_the_note_when_no_marker_applied() {
        let md = render(
            &baseline_diff(),
            &RenderOptions::default(),
            OutputFormat::Markdown,
        )
        .unwrap();
        assert!(!md.contains("region(s)"), "{md}");
    }

    /// The structured views must agree with the markdown: `delta` is the masked
    /// value, and `after`/`total_after` stay real, with the effective value
    /// alongside so a consumer can see why they differ.
    #[test]
    fn json_reports_markers_and_effective_coverage() {
        let mut diff = sample_diff();
        diff.has_baseline = true;
        diff.total_before = Some(92.8012);
        diff.total_after = Some(92.7924);
        diff.total_after_effective = Some(92.8012);
        diff.file_deltas = vec![FileDelta {
            path: "src/util/simd/x86.rs".to_string(),
            before: Some(90.0),
            after: Some(80.0),
            after_effective: Some(90.0),
        }];
        diff.markers = vec![tolerated_marker()];
        let json = render(&diff, &RenderOptions::default(), OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["project_delta"]["total_after"], 92.79);
        assert_eq!(value["project_delta"]["total_after_effective"], 92.8);
        let file = &value["project_delta"]["files"][0];
        assert_eq!(file["after"], 80.0);
        assert_eq!(file["after_effective"], 90.0);
        assert_eq!(
            file["delta"], 0.0,
            "delta is computed from the masked value"
        );
        assert_eq!(value["markers"][0]["kind"], "tolerate");
        assert_eq!(value["markers"][0]["side"], "both");
        assert_eq!(value["markers"][0]["start"], 41);
    }

    /// An unmasked run must not grow the effective fields — they exist only to
    /// explain a discrepancy, so an absent one means "there was none".
    #[test]
    fn json_omits_effective_fields_when_nothing_was_masked() {
        let mut diff = baseline_diff();
        diff.total_after_effective = diff.total_after;
        let json = render(&diff, &RenderOptions::default(), OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["project_delta"]
            .get("total_after_effective")
            .is_none());
        assert!(value["project_delta"]["files"][0]
            .get("after_effective")
            .is_none());
        assert!(value.get("markers").is_none());
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
                FileDelta::new("src/new.rs".to_string(), None, Some(50.0)),
                FileDelta::new("src/down.rs".to_string(), Some(100.0), Some(70.0)),
                FileDelta::new("src/up.rs".to_string(), Some(70.0), Some(90.0)),
                // below EPS → filtered out
                FileDelta::new("src/tiny.rs", Some(90.0), Some(90.02)),
                // `After` renders as an em dash
                FileDelta::new("src/gone.rs", Some(50.0), None),
            ],
            ..Default::default()
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

    #[test]
    fn markdown_renders_notable_unchanged_note() {
        let mut diff = baseline_diff();
        diff.notable_unchanged = vec![
            FileDelta::new("src/other.rs".to_string(), Some(80.0), Some(60.0)),
            // Absent from the baseline → delta() is None → renders as "🆕 new".
            FileDelta::new("src/fresh.rs".to_string(), None, Some(55.0)),
        ];
        let md = render(&diff, &RenderOptions::default(), OutputFormat::Markdown).unwrap();
        assert!(md.contains("unchanged file(s) also moved (not attributed to this PR)"));
        assert!(md.contains("`src/other.rs`"));
        assert!(md.contains("🔴 -20 pp"));
        assert!(md.contains("| `src/fresh.rs` | — | 55% | 🆕 new |"));
    }

    #[test]
    fn json_includes_notable_unchanged() {
        let mut diff = baseline_diff();
        diff.notable_unchanged = vec![FileDelta::new(
            "src/other.rs".to_string(),
            Some(80.0),
            Some(60.0),
        )];
        let json = render(&diff, &RenderOptions::default(), OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["project_delta"]["notable_unchanged"][0]["path"],
            "src/other.rs"
        );
    }
}
