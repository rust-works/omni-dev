//! Coverage attribution: combine a head per-line report with a [`DiffModel`]
//! (and optionally a baseline report) into the metrics a reviewer wants.
//!
//! - **Patch coverage** — of the lines this diff added, how many are covered.
//!   Needs only the head report + diff; immune to line-shift because added lines
//!   exist only in head.
//! - **Uncovered new lines** — the explicit `file:line` list of added lines that
//!   are not covered (the actionable output).
//! - **Project delta** — per-file and total before/after coverage *(baseline)*.
//! - **Indirect changes** — lines whose coverage flipped without their content
//!   changing, found by aligning base↔head through the diff *(baseline)*.

use std::collections::{BTreeMap, BTreeSet};

use super::diff::{DiffModel, FileDiff};
use super::markers::{FileMarkers, MarkerKind};
use super::model::{CoverageReport, FileCoverage};

/// A base-side → head-side line mapper used during indirect-change detection.
type BaseToHead<'a> = Box<dyn Fn(u32) -> Option<u32> + 'a>;

/// The source markers found on each side of the comparison.
///
/// `ignore` regions never reach here — they are applied as a filter on the
/// reports themselves before analysis, so their lines simply do not exist by
/// this point. What remains is `tolerate`, which needs the analysis to know
/// which head lines to score against the baseline instead of against the head
/// run.
///
/// Only the **head** tolerated set drives masking. The base side is carried for
/// reporting only: a tolerated base line whose region disappeared in head has
/// nothing left to mask, and one that survives is reached through its head
/// counterpart anyway.
#[derive(Debug, Clone, Default)]
pub struct Markers {
    /// Head-revision markers, keyed by repo-relative head path.
    pub head: BTreeMap<String, FileMarkers>,
    /// Base-revision markers, keyed by repo-relative base path.
    pub base: BTreeMap<String, FileMarkers>,
}

impl Markers {
    /// Whether either revision carried any marker at all.
    pub fn is_empty(&self) -> bool {
        self.head.values().all(FileMarkers::is_empty)
            && self.base.values().all(FileMarkers::is_empty)
    }

    /// The tolerated head lines of `path`, or an empty set.
    fn tolerated(&self, path: &str) -> Option<&BTreeSet<u32>> {
        self.head
            .get(path)
            .map(|m| &m.tolerated)
            .filter(|t| !t.is_empty())
    }
}

/// One region that actually applied, for the visibility note.
///
/// Silencing is never invisible: every applied region is reported with the
/// revision it was observed on, its span there, and its author's reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMarker {
    /// Repo-relative path of the marked file.
    pub path: String,
    /// Whether the region was ignored or tolerated.
    pub kind: MarkerKind,
    /// Which revision(s) the region was observed on.
    pub side: MarkerSide,
    /// First line of the region.
    pub start: u32,
    /// Last line of the region.
    pub end: u32,
    /// The marker's mandatory reason.
    pub reason: String,
}

/// Which revision a reported region was observed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerSide {
    /// Present identically on both revisions — the ordinary case for a region
    /// that neither moved nor changed.
    Both,
    /// Present only at head.
    Head,
    /// Present only at base.
    Base,
}

impl MarkerSide {
    /// Short label used in rendered output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Head => "head",
            Self::Base => "base",
        }
    }
}

/// Minimum net covered-line change for an *unchanged* file (one the diff never
/// touched) to be surfaced under [`DiffScope::DiffOnly`]. Small run-to-run flips
/// (the usual cross-run measurement noise) stay below this; a real cross-file
/// effect — e.g. a PR that removes a test, dropping a whole module's coverage —
/// exceeds it and is reported in `notable_unchanged`.
const NOTABLE_UNCHANGED_LINES: u64 = 10;

/// Which files the project-delta and indirect-change sections report on.
///
/// Coverage is measured by running the test suite twice (baseline vs head), and
/// that measurement is not perfectly reproducible — lines in code with any
/// run-to-run variance flip even when the source is identical. Only changes in
/// files the diff *touches* are causally attributable to the PR; everything else
/// is measurement noise. `DiffOnly` (the default) reports only touched files,
/// with a magnitude-gated note for substantially-moved unchanged files so real
/// cross-file effects still surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffScope {
    /// Report deltas/indirect only for files the diff touches (plus the
    /// `notable_unchanged` magnitude-gated note). The default.
    #[default]
    DiffOnly,
    /// Report deltas/indirect for *all* files (legacy; includes the cross-run
    /// measurement noise on files the PR never modified).
    All,
}

/// Covered / uncovered tally over a set of lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PatchCoverage {
    /// Lines covered (hit count > 0).
    pub covered: u64,
    /// Lines instrumented but uncovered (hit count == 0).
    pub uncovered: u64,
}

impl PatchCoverage {
    /// Instrumented lines considered (covered + uncovered).
    pub fn total(&self) -> u64 {
        self.covered + self.uncovered
    }

    /// Coverage percentage, or `None` when no instrumented lines were considered.
    pub fn percent(&self) -> Option<f64> {
        let total = self.total();
        if total == 0 {
            None
        } else {
            Some(self.covered as f64 / total as f64 * 100.0)
        }
    }
}

/// Patch coverage for a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// Repo-relative head path.
    pub path: String,
    /// Covered/uncovered tally over this file's added lines.
    pub patch: PatchCoverage,
    /// New-side line numbers that were added but are uncovered.
    pub uncovered_lines: Vec<u32>,
}

/// Per-file project coverage delta (requires a baseline report).
#[derive(Debug, Clone, PartialEq)]
pub struct FileDelta {
    /// Repo-relative head path.
    pub path: String,
    /// Baseline coverage percentage (`None` for a file new to head).
    pub before: Option<f64>,
    /// Head coverage percentage (`None` when the file has no executable lines).
    ///
    /// Always the **real, measured** value — this is what is displayed.
    pub after: Option<f64>,
    /// Head coverage with `tolerate` masking applied: tolerated lines scored
    /// with their baseline hit status instead of their head one.
    ///
    /// Equal to `after` unless a tolerated line actually flipped. It is what
    /// [`delta`](Self::delta) reports, so the *number* stays honest while the
    /// *signal* stops moving with cross-run variance.
    pub after_effective: Option<f64>,
}

impl FileDelta {
    /// Creates a delta whose effective coverage is its real coverage — the case
    /// for every file with no tolerated line.
    pub fn new(path: impl Into<String>, before: Option<f64>, after: Option<f64>) -> Self {
        Self {
            path: path.into(),
            before,
            after,
            after_effective: after,
        }
    }

    /// Percentage-point change, or `None` when there is no baseline value.
    ///
    /// Computed from [`after_effective`](Self::after_effective), so a flip on a
    /// tolerated line does not register as a change.
    pub fn delta(&self) -> Option<f64> {
        match (self.before, self.after_effective) {
            (Some(b), Some(a)) => Some(a - b),
            (Some(b), None) => Some(0.0 - b),
            _ => None,
        }
    }

    /// Whether masking changed this file's reported movement.
    pub fn is_masked(&self) -> bool {
        self.after_effective != self.after
    }
}

/// A line whose coverage status flipped without its content changing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndirectChange {
    /// Repo-relative head path.
    pub path: String,
    /// Base-side line number.
    pub base_line: u32,
    /// Head-side line number the base line maps to.
    pub head_line: u32,
    /// `true` if uncovered→covered, `false` if covered→uncovered.
    pub became_covered: bool,
}

/// The full attribution result.
#[derive(Debug, Clone, Default)]
pub struct CoverageDiff {
    /// Project-wide patch coverage.
    pub patch: PatchCoverage,
    /// Per-file patch coverage (only files with added, instrumented lines).
    pub file_patches: Vec<FilePatch>,
    /// Flattened actionable list of uncovered added lines.
    pub uncovered_new_lines: Vec<(String, u32)>,
    /// Whether a baseline report was supplied (enables the fields below).
    pub has_baseline: bool,
    /// Head project coverage percentage — the real, measured value, and the one
    /// that is displayed.
    pub total_after: Option<f64>,
    /// Head project coverage with `tolerate` masking applied, used for the
    /// headline direction and delta. Equal to `total_after` unless a tolerated
    /// line flipped. `None` without a baseline, where there is nothing to mask.
    pub total_after_effective: Option<f64>,
    /// Baseline project coverage percentage (requires a baseline).
    pub total_before: Option<f64>,
    /// Per-file project deltas (requires a baseline). Under [`DiffScope::DiffOnly`]
    /// this lists only files the diff touched.
    pub file_deltas: Vec<FileDelta>,
    /// Files the diff did *not* touch whose coverage nonetheless moved by at
    /// least [`NOTABLE_UNCHANGED_LINES`] covered lines (requires a baseline; only
    /// populated under [`DiffScope::DiffOnly`]). These are flagged separately as
    /// not attributable to the PR, so a real cross-file regression still shows
    /// while small measurement-noise flips stay hidden.
    pub notable_unchanged: Vec<FileDelta>,
    /// Indirect coverage flips on unchanged lines (requires a baseline). Under
    /// [`DiffScope::DiffOnly`] this lists only flips within files the diff touched.
    ///
    /// Flips on `tolerate`d head lines are excluded: they are precisely the
    /// cross-run variance the marker exists to silence.
    pub indirect: Vec<IndirectChange>,
    /// Source-marker regions that applied, for the visibility note. Empty when
    /// no marker was found.
    pub markers: Vec<AppliedMarker>,
}

impl CoverageDiff {
    /// Indirect lines that became covered.
    pub fn indirect_newly_covered(&self) -> usize {
        self.indirect.iter().filter(|c| c.became_covered).count()
    }

    /// Indirect lines that became uncovered.
    pub fn indirect_newly_uncovered(&self) -> usize {
        self.indirect.iter().filter(|c| !c.became_covered).count()
    }
}

/// Runs the full attribution at the given [`DiffScope`], with no source markers.
pub fn analyze(
    head: &CoverageReport,
    diff: &DiffModel,
    baseline: Option<&CoverageReport>,
    scope: DiffScope,
) -> CoverageDiff {
    analyze_with_markers(head, diff, baseline, scope, &Markers::default())
}

/// Runs the full attribution, applying `tolerate` source markers.
///
/// `ignore` markers are *not* handled here: they are a filter on the reports
/// themselves, applied before this is called, so their lines have already left
/// both sides. `markers` carries what remains — the tolerated line sets, and the
/// applied-region list for reporting.
///
/// Without a baseline, `tolerate` is inert: masking substitutes a *baseline* hit
/// status, and there is none.
pub fn analyze_with_markers(
    head: &CoverageReport,
    diff: &DiffModel,
    baseline: Option<&CoverageReport>,
    scope: DiffScope,
    markers: &Markers,
) -> CoverageDiff {
    let mut result = CoverageDiff {
        total_after: head.percent(),
        has_baseline: baseline.is_some(),
        markers: applied_markers(markers),
        ..Default::default()
    };

    patch_coverage(head, diff, &mut result);

    if let Some(baseline) = baseline {
        result.total_before = baseline.percent();
        project_delta(head, baseline, diff, scope, markers, &mut result);
        indirect_changes(head, baseline, diff, scope, markers, &mut result);
    }

    result
}

/// Flattens both revisions' markers into the reportable list, collapsing a
/// region that is identical on both sides — the ordinary case for a region that
/// neither moved nor changed — into a single `both` entry.
fn applied_markers(markers: &Markers) -> Vec<AppliedMarker> {
    let mut applied: Vec<AppliedMarker> = Vec::new();
    for (path, file) in &markers.head {
        for region in &file.regions {
            let same_at_base = markers
                .base
                .get(path)
                .is_some_and(|base| base.regions.iter().any(|other| other == region));
            applied.push(AppliedMarker {
                path: path.clone(),
                kind: region.kind,
                side: if same_at_base {
                    MarkerSide::Both
                } else {
                    MarkerSide::Head
                },
                start: region.start,
                end: region.end,
                reason: region.reason.clone(),
            });
        }
    }
    for (path, file) in &markers.base {
        for region in &file.regions {
            let seen_at_head = markers
                .head
                .get(path)
                .is_some_and(|head| head.regions.iter().any(|other| other == region));
            if seen_at_head {
                continue;
            }
            applied.push(AppliedMarker {
                path: path.clone(),
                kind: region.kind,
                side: MarkerSide::Base,
                start: region.start,
                end: region.end,
                reason: region.reason.clone(),
            });
        }
    }
    applied.sort_by(|a, b| a.path.cmp(&b.path).then(a.start.cmp(&b.start)));
    applied
}

/// Head-side hit statuses for one file with `tolerate` masking applied.
///
/// Returns the substitutions only — head line → the baseline hit status that
/// should stand in for its measured one — so a caller can leave every other line
/// alone.
///
/// The map is built by walking the **base** file forwards through the diff
/// alignment rather than inverting it: `map` is base → head, and every
/// substitution needs a base line anyway. A tolerated head line that no base
/// line maps onto therefore gets no entry, which is exactly the rule — a line
/// added inside a tolerated region keeps its real status, because there is no
/// baseline status to inherit.
fn tolerated_substitutions(
    base_file: &FileCoverage,
    map: &BaseToHead<'_>,
    tolerated: &BTreeSet<u32>,
) -> BTreeMap<u32, u64> {
    let mut substitutions = BTreeMap::new();
    for (&base_line, &base_hits) in &base_file.lines {
        let Some(head_line) = map(base_line) else {
            continue;
        };
        if tolerated.contains(&head_line) {
            substitutions.insert(head_line, base_hits);
        }
    }
    substitutions
}

/// Covered-line count for `file` with `substitutions` standing in for the
/// measured hit status of the lines they name.
fn effective_covered(file: &FileCoverage, substitutions: &BTreeMap<u32, u64>) -> u64 {
    file.lines
        .iter()
        .filter(|(line, hits)| {
            let effective = substitutions.get(line).unwrap_or(hits);
            *effective > 0
        })
        .count() as u64
}

/// Computes patch coverage and the uncovered-new-line list.
fn patch_coverage(head: &CoverageReport, diff: &DiffModel, result: &mut CoverageDiff) {
    for file in diff.files.values() {
        let mut patch = PatchCoverage::default();
        let mut uncovered_lines = Vec::new();
        for &line in &file.added {
            match head.hits(&file.new_path, line) {
                Some(h) if h > 0 => patch.covered += 1,
                Some(_) => {
                    patch.uncovered += 1;
                    uncovered_lines.push(line);
                }
                // Not instrumented (blank/comment/non-executable): excluded.
                None => {}
            }
        }
        if patch.total() == 0 {
            continue;
        }
        result.patch.covered += patch.covered;
        result.patch.uncovered += patch.uncovered;
        for &line in &uncovered_lines {
            result
                .uncovered_new_lines
                .push((file.new_path.clone(), line));
        }
        result.file_patches.push(FilePatch {
            path: file.new_path.clone(),
            patch,
            uncovered_lines,
        });
    }

    result.file_patches.sort_by(|a, b| a.path.cmp(&b.path));
    result
        .uncovered_new_lines
        .sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
}

/// Computes per-file project deltas against the baseline.
///
/// Under [`DiffScope::DiffOnly`], a file the diff did not touch goes to
/// `file_deltas` only if its coverage moved by at least
/// [`NOTABLE_UNCHANGED_LINES`] covered lines (→ `notable_unchanged`); smaller
/// moves are dropped as measurement noise.
fn project_delta(
    head: &CoverageReport,
    baseline: &CoverageReport,
    diff: &DiffModel,
    scope: DiffScope,
    markers: &Markers,
    result: &mut CoverageDiff,
) {
    let by_old_path = index_by_old_path(diff);
    let mut effective_covered_total = 0_u64;

    for (path, file) in &head.files {
        // A tolerated head line is scored with its base counterpart's status,
        // which needs both a baseline file and an alignment onto it.
        let substitutions = markers
            .tolerated(path)
            .and_then(|tolerated| {
                let (base_path, map) = base_side(path, diff, &by_old_path)?;
                let base_file = baseline.files.get(&base_path)?;
                Some(tolerated_substitutions(base_file, &map, tolerated))
            })
            .unwrap_or_default();

        let covered_after = file.covered_lines();
        let covered_effective = if substitutions.is_empty() {
            covered_after
        } else {
            effective_covered(file, &substitutions)
        };
        effective_covered_total += covered_effective;

        let total = file.total_lines();
        let percent = |covered: u64| (total > 0).then(|| covered as f64 / total as f64 * 100.0);
        let delta = FileDelta {
            path: path.clone(),
            before: baseline.files.get(path).and_then(FileCoverage::percent),
            after: percent(covered_after),
            after_effective: percent(covered_effective),
        };

        if scope == DiffScope::All || diff.files.contains_key(path) {
            result.file_deltas.push(delta);
            continue;
        }

        // Untouched file under DiffOnly: surface only a substantial net move,
        // measured on the effective count so a fully-tolerated flip cannot
        // reach the threshold.
        let covered_before = baseline
            .files
            .get(path)
            .map_or(0, FileCoverage::covered_lines);
        let net = covered_effective.abs_diff(covered_before);
        if net >= NOTABLE_UNCHANGED_LINES {
            result.notable_unchanged.push(delta);
        }
    }

    let total_lines = head.total_lines();
    result.total_after_effective =
        (total_lines > 0).then(|| effective_covered_total as f64 / total_lines as f64 * 100.0);

    result.file_deltas.sort_by(|a, b| a.path.cmp(&b.path));
    result.notable_unchanged.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Indexes the diff's changed files by their base-side path.
fn index_by_old_path(diff: &DiffModel) -> BTreeMap<&str, &FileDiff> {
    diff.files
        .values()
        .filter_map(|f| f.old_path.as_deref().map(|p| (p, f)))
        .collect()
}

/// The base path and base→head alignment for a **head** path.
///
/// A file the diff never touched aligns by identity — the case that matters
/// most, since a CPU-gated region flips in files no PR touches. A file the diff
/// added has no base counterpart at all.
fn base_side<'a>(
    head_path: &str,
    diff: &'a DiffModel,
    by_old_path: &BTreeMap<&'a str, &'a FileDiff>,
) -> Option<(String, BaseToHead<'a>)> {
    match diff.files.get(head_path) {
        Some(fd) if fd.is_new => None,
        Some(fd) => {
            let old_path = fd.old_path.clone()?;
            Some((old_path, Box::new(move |l| fd.map_base_to_head(l))))
        }
        None => {
            // Untouched by the diff — unless it is the *target* of a rename,
            // in which case `diff.files` would have held it. Identity aligns.
            let _ = by_old_path;
            Some((head_path.to_string(), Box::new(Some)))
        }
    }
}

/// Detects coverage flips on lines whose content did not change.
///
/// Changed files are aligned through their [`FileDiff`]. Under [`DiffScope::All`],
/// entirely-unchanged files are also compared by identity alignment; under
/// [`DiffScope::DiffOnly`] (the default) they are skipped, because a per-line
/// flip in a file the PR never touched is cross-run measurement noise, not a
/// real change (its file-level move, if substantial, is reported via
/// `notable_unchanged` instead).
fn indirect_changes(
    head: &CoverageReport,
    baseline: &CoverageReport,
    diff: &DiffModel,
    scope: DiffScope,
    markers: &Markers,
    result: &mut CoverageDiff,
) {
    let by_old_path = index_by_old_path(diff);

    for (base_path, base_file) in &baseline.files {
        // Determine the head path and the base→head line mapping.
        let (new_path, map): (&str, BaseToHead<'_>) =
            if let Some(fd) = by_old_path.get(base_path.as_str()) {
                let fd = *fd;
                (
                    fd.new_path.as_str(),
                    Box::new(move |l| fd.map_base_to_head(l)),
                )
            } else if scope == DiffScope::All
                && head.files.contains_key(base_path)
                && !diff.files.contains_key(base_path)
            {
                // File untouched by the diff: identity alignment. (A file added by
                // the diff is excluded — its lines are direct, not indirect.)
                // Only under `All` scope — otherwise these per-line flips are noise.
                (base_path.as_str(), Box::new(Some))
            } else {
                // Deleted in head — nothing to compare.
                continue;
            };

        for (&base_line, &base_hits) in &base_file.lines {
            let Some(head_line) = map(base_line) else {
                continue;
            };
            let Some(head_hits) = head.hits(new_path, head_line) else {
                continue;
            };
            // A tolerated head line's flip is the variance the marker exists
            // to silence; reporting it would put back exactly what was masked.
            if markers
                .tolerated(new_path)
                .is_some_and(|t| t.contains(&head_line))
            {
                continue;
            }
            let covered_before = base_hits > 0;
            let covered_after = head_hits > 0;
            if covered_before != covered_after {
                result.indirect.push(IndirectChange {
                    path: new_path.to_string(),
                    base_line,
                    head_line,
                    became_covered: covered_after,
                });
            }
        }
    }

    result
        .indirect
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.head_line.cmp(&b.head_line)));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::coverage::model::FileCoverage;
    use std::collections::{BTreeMap, BTreeSet};

    pub(super) fn report(files: &[(&str, &[(u32, u64)])]) -> CoverageReport {
        let mut r = CoverageReport::new();
        for (path, lines) in files {
            let mut f = FileCoverage::new(*path);
            for &(n, h) in *lines {
                f.record(n, h);
            }
            r.insert(f);
        }
        r
    }

    /// Minimal diff with one added-line set on a (possibly new) file.
    pub(super) fn diff_added(path: &str, is_new: bool, added: &[u32]) -> DiffModel {
        let old_path = if is_new { None } else { Some(path.to_string()) };
        let fd = FileDiff::new(
            path,
            old_path,
            is_new,
            false,
            added.iter().copied().collect::<BTreeSet<u32>>(),
            BTreeSet::new(),
        );
        let mut files = BTreeMap::new();
        files.insert(path.to_string(), fd);
        DiffModel { files }
    }

    #[test]
    fn patch_coverage_counts_added_lines_only() {
        // File has lines 1..4; the diff added lines 2 and 3.
        let head = report(&[("src/a.rs", &[(1, 1), (2, 1), (3, 0), (4, 1)])]);
        let diff = diff_added("src/a.rs", false, &[2, 3]);
        let out = analyze(&head, &diff, None, DiffScope::All);
        assert_eq!(
            out.patch,
            PatchCoverage {
                covered: 1,
                uncovered: 1
            }
        );
        assert_eq!(out.patch.percent(), Some(50.0));
        assert_eq!(out.uncovered_new_lines, vec![("src/a.rs".to_string(), 3)]);
    }

    #[test]
    fn added_non_executable_lines_excluded_from_denominator() {
        // Added lines 2 (uncovered), 5 (not instrumented — absent from report).
        let head = report(&[("src/a.rs", &[(1, 1), (2, 0)])]);
        let diff = diff_added("src/a.rs", false, &[2, 5]);
        let out = analyze(&head, &diff, None, DiffScope::All);
        assert_eq!(
            out.patch,
            PatchCoverage {
                covered: 0,
                uncovered: 1
            }
        );
    }

    #[test]
    fn new_file_patch_coverage() {
        let head = report(&[("src/new.rs", &[(1, 1), (2, 0), (3, 1)])]);
        let diff = diff_added("src/new.rs", true, &[1, 2, 3]);
        let out = analyze(&head, &diff, None, DiffScope::All);
        assert_eq!(
            out.patch,
            PatchCoverage {
                covered: 2,
                uncovered: 1
            }
        );
        assert_eq!(out.file_patches.len(), 1);
        assert_eq!(out.file_patches[0].uncovered_lines, vec![2]);
    }

    #[test]
    fn project_delta_with_baseline() {
        let baseline = report(&[("src/a.rs", &[(1, 1), (2, 0)])]); // 50%
        let head = report(&[("src/a.rs", &[(1, 1), (2, 1)])]); // 100%
        let diff = diff_added("src/a.rs", false, &[2]);
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::All);
        assert!(out.has_baseline);
        assert_eq!(out.total_before, Some(50.0));
        assert_eq!(out.total_after, Some(100.0));
        assert_eq!(out.file_deltas.len(), 1);
        assert_eq!(out.file_deltas[0].delta(), Some(50.0));
    }

    #[test]
    fn delta_for_new_file_is_after_minus_nothing() {
        let baseline = report(&[]);
        let head = report(&[("src/new.rs", &[(1, 1)])]);
        let diff = diff_added("src/new.rs", true, &[1]);
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::All);
        assert_eq!(out.file_deltas[0].before, None);
        assert_eq!(out.file_deltas[0].after, Some(100.0));
    }

    #[test]
    fn indirect_change_on_unchanged_file() {
        // File src/b.rs is untouched by the diff but line 5 lost coverage.
        let baseline = report(&[("src/b.rs", &[(5, 3)])]);
        let head = report(&[("src/b.rs", &[(5, 0)])]);
        let diff = diff_added("src/a.rs", true, &[1]); // unrelated change
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::All);
        assert_eq!(out.indirect.len(), 1);
        assert_eq!(out.indirect[0].path, "src/b.rs");
        assert_eq!(out.indirect[0].base_line, 5);
        assert!(!out.indirect[0].became_covered);
        assert_eq!(out.indirect_newly_uncovered(), 1);
    }

    #[test]
    fn patch_percent_none_when_empty() {
        assert_eq!(PatchCoverage::default().percent(), None);
        assert_eq!(PatchCoverage::default().total(), 0);
    }

    #[test]
    fn file_delta_handles_all_combinations() {
        let d = |before, after| FileDelta::new("x", before, after);
        assert_eq!(d(Some(80.0), Some(90.0)).delta(), Some(10.0));
        assert_eq!(d(Some(50.0), None).delta(), Some(-50.0));
        assert_eq!(d(None, Some(50.0)).delta(), None);
    }

    #[test]
    fn indirect_change_newly_covered() {
        let baseline = report(&[("src/b.rs", &[(5, 0)])]);
        let head = report(&[("src/b.rs", &[(5, 3)])]);
        let diff = diff_added("src/a.rs", true, &[1]);
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::All);
        assert_eq!(out.indirect_newly_covered(), 1);
        assert!(out.indirect[0].became_covered);
    }

    #[test]
    fn added_lines_are_not_counted_as_indirect() {
        // The added line 1 is direct (patch), not indirect, even with a baseline.
        let baseline = report(&[("src/a.rs", &[(1, 1)])]);
        let head = report(&[("src/a.rs", &[(1, 0)])]);
        let diff = diff_added("src/a.rs", true, &[1]); // new file → no old_path
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::All);
        // New file has no base mapping, so no indirect entries from it.
        assert!(out.indirect.is_empty());
    }

    // ── DiffScope::DiffOnly (noise filter) ──

    #[test]
    fn diff_only_suppresses_untouched_file_indirect() {
        // Same as indirect_change_on_unchanged_file, but DiffOnly drops the flip.
        let baseline = report(&[("src/b.rs", &[(5, 3)])]);
        let head = report(&[("src/b.rs", &[(5, 0)])]);
        let diff = diff_added("src/a.rs", true, &[1]); // unrelated change
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);
        assert!(
            out.indirect.is_empty(),
            "an untouched-file flip is cross-run noise under DiffOnly"
        );
        // A one-line move is below the notable threshold → not surfaced.
        assert!(out.notable_unchanged.is_empty());
    }

    #[test]
    fn diff_only_delta_table_scoped_to_changed_files() {
        let baseline = report(&[
            ("src/a.rs", &[(1, 1), (2, 0)]),
            ("src/b.rs", &[(1, 1), (2, 1)]),
        ]);
        let head = report(&[
            ("src/a.rs", &[(1, 1), (2, 1)]),
            ("src/b.rs", &[(1, 1), (2, 0)]),
        ]);
        let diff = diff_added("src/a.rs", false, &[2]); // only a.rs is touched
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);
        let paths: Vec<&str> = out.file_deltas.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs"], "only the changed file appears");
        assert!(out.notable_unchanged.is_empty(), "b.rs moved < threshold");
    }

    #[test]
    fn diff_only_surfaces_substantial_unchanged_move() {
        // An untouched file loses 12 covered lines (e.g. its only test was removed).
        let before: Vec<(u32, u64)> = (1..=12).map(|n| (n, 1)).collect();
        let after: Vec<(u32, u64)> = (1..=12).map(|n| (n, 0)).collect();
        let baseline = report(&[("src/c.rs", &before)]);
        let head = report(&[("src/c.rs", &after)]);
        let diff = diff_added("src/a.rs", true, &[1]); // unrelated
        let out = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);
        assert!(out.file_deltas.is_empty(), "c.rs is not in the diff");
        assert_eq!(
            out.notable_unchanged.len(),
            1,
            "12-line drop exceeds threshold"
        );
        assert_eq!(out.notable_unchanged[0].path, "src/c.rs");
        assert!(
            out.indirect.is_empty(),
            "per-line indirect still suppressed"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod marker_tests {
    use super::tests::*;
    use super::*;
    use crate::coverage::markers::Region;

    /// Builds head-side markers tolerating `lines` of `path`.
    fn tolerate(path: &str, lines: &[u32]) -> Markers {
        let regions = lines
            .iter()
            .map(|&line| Region {
                kind: MarkerKind::Tolerate,
                start: line,
                end: line,
                reason: "CPU-gated".to_string(),
            })
            .collect();
        Markers {
            head: BTreeMap::from([(path.to_string(), FileMarkers::new(regions))]),
            base: BTreeMap::new(),
        }
    }

    /// The motivating case: an untouched, CPU-gated file whose lines flip
    /// between two runner CPUs. Without the marker the headline moves; with it
    /// the headline is flat and the reported percentage is unchanged.
    #[test]
    fn tolerated_flip_in_an_untouched_file_does_not_move_the_headline() {
        // `src/gated.rs` lines 1-2 covered at base, uncovered at head.
        let head = report(&[
            ("src/gated.rs", &[(1, 0), (2, 0)]),
            ("src/other.rs", &[(1, 1), (2, 1)]),
        ]);
        let baseline = report(&[
            ("src/gated.rs", &[(1, 5), (2, 5)]),
            ("src/other.rs", &[(1, 1), (2, 1)]),
        ]);
        let diff = DiffModel::default();

        let bare = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);
        assert_eq!(bare.total_after, Some(50.0));
        assert_eq!(bare.total_after_effective, Some(50.0));
        assert_eq!(bare.total_before, Some(100.0));

        let markers = tolerate("src/gated.rs", &[1, 2]);
        let masked =
            analyze_with_markers(&head, &diff, Some(&baseline), DiffScope::DiffOnly, &markers);
        assert_eq!(
            masked.total_after,
            Some(50.0),
            "the reported percentage must stay the real measured value"
        );
        assert_eq!(
            masked.total_after_effective,
            Some(100.0),
            "the headline delta must see the baseline status of tolerated lines"
        );
    }

    /// Masking is not a blanket amnesty: an untolerated line in the same file
    /// still moves the number.
    #[test]
    fn untolerated_lines_in_a_tolerated_file_still_count() {
        let head = report(&[("src/gated.rs", &[(1, 0), (2, 0)])]);
        let baseline = report(&[("src/gated.rs", &[(1, 5), (2, 5)])]);
        let markers = tolerate("src/gated.rs", &[1]);
        let out = analyze_with_markers(
            &head,
            &DiffModel::default(),
            Some(&baseline),
            DiffScope::DiffOnly,
            &markers,
        );
        assert_eq!(out.total_after, Some(0.0));
        assert_eq!(out.total_after_effective, Some(50.0));
    }

    /// A tolerated line with *no* base counterpart — one the diff added — keeps
    /// its real status. New code should still be tested even if it flaps later.
    #[test]
    fn a_tolerated_added_line_keeps_its_real_status() {
        let head = report(&[("src/a.rs", &[(1, 1), (2, 0)])]);
        let baseline = report(&[("src/a.rs", &[(1, 1)])]);
        let diff = diff_added("src/a.rs", false, &[2]);
        let markers = tolerate("src/a.rs", &[2]);
        let out =
            analyze_with_markers(&head, &diff, Some(&baseline), DiffScope::DiffOnly, &markers);
        assert_eq!(
            out.total_after_effective,
            Some(50.0),
            "an added line has no baseline status to inherit"
        );
        assert_eq!(out.patch.covered, 0);
        assert_eq!(
            out.patch.uncovered, 1,
            "a tolerated added line stays in the patch denominator"
        );
    }

    /// The per-file table displays the real coverage while its delta is masked.
    #[test]
    fn per_file_delta_is_masked_but_the_percentage_is_real() {
        let head = report(&[("src/a.rs", &[(1, 0), (2, 1)])]);
        let baseline = report(&[("src/a.rs", &[(1, 5), (2, 1)])]);
        let diff = diff_added("src/a.rs", false, &[]);
        let markers = tolerate("src/a.rs", &[1]);
        let out =
            analyze_with_markers(&head, &diff, Some(&baseline), DiffScope::DiffOnly, &markers);
        let fd = &out.file_deltas[0];
        assert_eq!(fd.after, Some(50.0), "displayed percentage stays real");
        assert_eq!(fd.after_effective, Some(100.0));
        assert_eq!(fd.delta(), Some(0.0));
        assert!(fd.is_masked());
    }

    /// The notable-unchanged gate counts effective covered lines, so a
    /// fully-tolerated flip cannot reach the threshold.
    #[test]
    fn tolerated_flip_does_not_reach_the_notable_threshold() {
        let lines_head: Vec<(u32, u64)> = (1..=12).map(|n| (n, 0)).collect();
        let lines_base: Vec<(u32, u64)> = (1..=12).map(|n| (n, 3)).collect();
        let head = report(&[("src/gated.rs", &lines_head)]);
        let baseline = report(&[("src/gated.rs", &lines_base)]);
        let diff = DiffModel::default();

        let bare = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);
        assert_eq!(bare.notable_unchanged.len(), 1, "12 lines flipped");

        let all: Vec<u32> = (1..=12).collect();
        let markers = tolerate("src/gated.rs", &all);
        let masked =
            analyze_with_markers(&head, &diff, Some(&baseline), DiffScope::DiffOnly, &markers);
        assert!(masked.notable_unchanged.is_empty());
    }

    /// A flip on a tolerated line must not come back as an indirect change —
    /// that would put back exactly what was masked.
    #[test]
    fn indirect_changes_skip_tolerated_lines() {
        let head = report(&[("src/a.rs", &[(1, 0), (2, 0)])]);
        let baseline = report(&[("src/a.rs", &[(1, 5), (2, 5)])]);
        let diff = diff_added("src/a.rs", false, &[]);

        let bare = analyze(&head, &diff, Some(&baseline), DiffScope::DiffOnly);
        assert_eq!(bare.indirect.len(), 2);

        let markers = tolerate("src/a.rs", &[1]);
        let masked =
            analyze_with_markers(&head, &diff, Some(&baseline), DiffScope::DiffOnly, &markers);
        assert_eq!(masked.indirect.len(), 1);
        assert_eq!(masked.indirect[0].head_line, 2);
    }

    /// Masking substitutes a *baseline* status, so with no baseline there is
    /// nothing to substitute and `tolerate` is inert.
    #[test]
    fn tolerate_is_inert_without_a_baseline() {
        let head = report(&[("src/a.rs", &[(1, 0), (2, 1)])]);
        let markers = tolerate("src/a.rs", &[1]);
        let out = analyze_with_markers(
            &head,
            &diff_added("src/a.rs", false, &[]),
            None,
            DiffScope::DiffOnly,
            &markers,
        );
        assert_eq!(out.total_after, Some(50.0));
        assert_eq!(out.total_after_effective, None);
    }

    /// A region present identically on both revisions collapses to one `both`
    /// entry; one that exists on only one side is reported against that side.
    #[test]
    fn applied_markers_collapse_when_identical_on_both_sides() {
        let shared = Region {
            kind: MarkerKind::Tolerate,
            start: 3,
            end: 5,
            reason: "CPU-gated".to_string(),
        };
        let base_only = Region {
            kind: MarkerKind::Ignore,
            start: 9,
            end: 9,
            reason: "removed in head".to_string(),
        };
        let markers = Markers {
            head: BTreeMap::from([(
                "src/a.rs".to_string(),
                FileMarkers::new(vec![shared.clone()]),
            )]),
            base: BTreeMap::from([(
                "src/a.rs".to_string(),
                FileMarkers::new(vec![shared, base_only]),
            )]),
        };
        let out = analyze_with_markers(
            &report(&[("src/a.rs", &[(1, 1)])]),
            &DiffModel::default(),
            None,
            DiffScope::DiffOnly,
            &markers,
        );
        assert_eq!(out.markers.len(), 2);
        assert_eq!(out.markers[0].side, MarkerSide::Both);
        assert_eq!(out.markers[0].start, 3);
        assert_eq!(out.markers[1].side, MarkerSide::Base);
        assert_eq!(out.markers[1].kind, MarkerKind::Ignore);
    }
}
