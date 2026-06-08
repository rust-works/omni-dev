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

use std::collections::BTreeMap;

use super::diff::{DiffModel, FileDiff};
use super::model::{CoverageReport, FileCoverage};

/// A base-side → head-side line mapper used during indirect-change detection.
type BaseToHead<'a> = Box<dyn Fn(u32) -> Option<u32> + 'a>;

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
    pub after: Option<f64>,
}

impl FileDelta {
    /// Percentage-point change, or `None` when there is no baseline value.
    pub fn delta(&self) -> Option<f64> {
        match (self.before, self.after) {
            (Some(b), Some(a)) => Some(a - b),
            (Some(b), None) => Some(0.0 - b),
            _ => None,
        }
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
    /// Head project coverage percentage.
    pub total_after: Option<f64>,
    /// Baseline project coverage percentage (requires a baseline).
    pub total_before: Option<f64>,
    /// Per-file project deltas (requires a baseline).
    pub file_deltas: Vec<FileDelta>,
    /// Indirect coverage flips on unchanged lines (requires a baseline).
    pub indirect: Vec<IndirectChange>,
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

/// Runs the full attribution.
pub fn analyze(
    head: &CoverageReport,
    diff: &DiffModel,
    baseline: Option<&CoverageReport>,
) -> CoverageDiff {
    let mut result = CoverageDiff {
        total_after: head.percent(),
        has_baseline: baseline.is_some(),
        ..Default::default()
    };

    patch_coverage(head, diff, &mut result);

    if let Some(baseline) = baseline {
        result.total_before = baseline.percent();
        project_delta(head, baseline, &mut result);
        indirect_changes(head, baseline, diff, &mut result);
    }

    result
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

/// Computes per-file and total project deltas against the baseline.
fn project_delta(head: &CoverageReport, baseline: &CoverageReport, result: &mut CoverageDiff) {
    for (path, file) in &head.files {
        result.file_deltas.push(FileDelta {
            path: path.clone(),
            before: baseline.files.get(path).and_then(FileCoverage::percent),
            after: file.percent(),
        });
    }
    result.file_deltas.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Detects coverage flips on lines whose content did not change.
///
/// Covers both changed files (aligned through their [`FileDiff`]) and entirely
/// unchanged files (identity alignment), since coverage can shift on a file the
/// diff never touched (e.g. a callee changed).
fn indirect_changes(
    head: &CoverageReport,
    baseline: &CoverageReport,
    diff: &DiffModel,
    result: &mut CoverageDiff,
) {
    // Index changed files by their base-side path.
    let by_old_path: BTreeMap<&str, &FileDiff> = diff
        .files
        .values()
        .filter_map(|f| f.old_path.as_deref().map(|p| (p, f)))
        .collect();

    for (base_path, base_file) in &baseline.files {
        // Determine the head path and the base→head line mapping.
        let (new_path, map): (&str, BaseToHead<'_>) =
            if let Some(fd) = by_old_path.get(base_path.as_str()) {
                let fd = *fd;
                (
                    fd.new_path.as_str(),
                    Box::new(move |l| fd.map_base_to_head(l)),
                )
            } else if head.files.contains_key(base_path) && !diff.files.contains_key(base_path) {
                // File untouched by the diff: identity alignment. (A file added by
                // the diff is excluded — its lines are direct, not indirect.)
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

    fn report(files: &[(&str, &[(u32, u64)])]) -> CoverageReport {
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
    fn diff_added(path: &str, is_new: bool, added: &[u32]) -> DiffModel {
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
        let out = analyze(&head, &diff, None);
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
        let out = analyze(&head, &diff, None);
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
        let out = analyze(&head, &diff, None);
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
        let out = analyze(&head, &diff, Some(&baseline));
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
        let out = analyze(&head, &diff, Some(&baseline));
        assert_eq!(out.file_deltas[0].before, None);
        assert_eq!(out.file_deltas[0].after, Some(100.0));
    }

    #[test]
    fn indirect_change_on_unchanged_file() {
        // File src/b.rs is untouched by the diff but line 5 lost coverage.
        let baseline = report(&[("src/b.rs", &[(5, 3)])]);
        let head = report(&[("src/b.rs", &[(5, 0)])]);
        let diff = diff_added("src/a.rs", true, &[1]); // unrelated change
        let out = analyze(&head, &diff, Some(&baseline));
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
        let d = |before, after| FileDelta {
            path: "x".to_string(),
            before,
            after,
        };
        assert_eq!(d(Some(80.0), Some(90.0)).delta(), Some(10.0));
        assert_eq!(d(Some(50.0), None).delta(), Some(-50.0));
        assert_eq!(d(None, Some(50.0)).delta(), None);
    }

    #[test]
    fn indirect_change_newly_covered() {
        let baseline = report(&[("src/b.rs", &[(5, 0)])]);
        let head = report(&[("src/b.rs", &[(5, 3)])]);
        let diff = diff_added("src/a.rs", true, &[1]);
        let out = analyze(&head, &diff, Some(&baseline));
        assert_eq!(out.indirect_newly_covered(), 1);
        assert!(out.indirect[0].became_covered);
    }

    #[test]
    fn added_lines_are_not_counted_as_indirect() {
        // The added line 1 is direct (patch), not indirect, even with a baseline.
        let baseline = report(&[("src/a.rs", &[(1, 1)])]);
        let head = report(&[("src/a.rs", &[(1, 0)])]);
        let diff = diff_added("src/a.rs", true, &[1]); // new file → no old_path
        let out = analyze(&head, &diff, Some(&baseline));
        // New file has no base mapping, so no indirect entries from it.
        assert!(out.indirect.is_empty());
    }
}
