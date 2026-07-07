//! The per-line coverage model that every parser produces.
//!
//! A [`CoverageReport`] is a map of repo-relative file paths to their
//! [`FileCoverage`], where each file records the hit count of every
//! *executable* line. Non-executable lines (blank, comment, declaration-only)
//! are simply absent — [`CoverageReport::hits`] returns `None` for them, which
//! callers use to exclude them from coverage denominators.

use std::collections::BTreeMap;
use std::path::Path;

/// Per-line hit counts for a single source file.
///
/// Only executable lines are present in `lines`; a line absent from the map is
/// not instrumented (blank, comment, etc.) and must not be counted towards
/// coverage totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCoverage {
    /// Repo-relative path of the source file.
    pub path: String,
    /// Map of 1-based line number to hit count.
    pub lines: BTreeMap<u32, u64>,
}

impl FileCoverage {
    /// Creates an empty file coverage for `path`.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            lines: BTreeMap::new(),
        }
    }

    /// Records `hits` for `line`. Repeated records for the same line take the
    /// maximum, so a line covered by any region counts as covered.
    pub fn record(&mut self, line: u32, hits: u64) {
        self.lines
            .entry(line)
            .and_modify(|h| *h = (*h).max(hits))
            .or_insert(hits);
    }

    /// Number of executable lines.
    pub fn total_lines(&self) -> u64 {
        self.lines.len() as u64
    }

    /// Number of executable lines hit at least once.
    pub fn covered_lines(&self) -> u64 {
        self.lines.values().filter(|&&h| h > 0).count() as u64
    }

    /// Line coverage percentage, or `None` when the file has no executable lines.
    pub fn percent(&self) -> Option<f64> {
        let total = self.total_lines();
        if total == 0 {
            None
        } else {
            Some(self.covered_lines() as f64 / total as f64 * 100.0)
        }
    }
}

/// A whole coverage report: repo-relative file path → [`FileCoverage`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageReport {
    /// Per-file coverage, keyed by repo-relative path.
    pub files: BTreeMap<String, FileCoverage>,
}

impl CoverageReport {
    /// Creates an empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or merges a file's coverage into the report.
    ///
    /// If the path already exists, line hit counts are merged (taking the max
    /// per line), which keeps the model robust against reports that split one
    /// file across multiple records.
    pub fn insert(&mut self, file: FileCoverage) {
        match self.files.get_mut(&file.path) {
            Some(existing) => {
                for (line, hits) in file.lines {
                    existing.record(line, hits);
                }
            }
            None => {
                self.files.insert(file.path.clone(), file);
            }
        }
    }

    /// Hit count for `path`:`line`, or `None` when the line is not instrumented
    /// (or the file is absent from the report).
    pub fn hits(&self, path: &str, line: u32) -> Option<u64> {
        self.files
            .get(path)
            .and_then(|f| f.lines.get(&line).copied())
    }

    /// Total executable lines across all files.
    pub fn total_lines(&self) -> u64 {
        self.files.values().map(FileCoverage::total_lines).sum()
    }

    /// Total covered lines across all files.
    pub fn covered_lines(&self) -> u64 {
        self.files.values().map(FileCoverage::covered_lines).sum()
    }

    /// Project-wide line coverage percentage, or `None` when there are no
    /// executable lines.
    pub fn percent(&self) -> Option<f64> {
        let total = self.total_lines();
        if total == 0 {
            None
        } else {
            Some(self.covered_lines() as f64 / total as f64 * 100.0)
        }
    }

    /// Normalises every file path to be repo-relative.
    ///
    /// Coverage tools usually emit absolute paths (lcov `SF:`, llvm-cov
    /// `filename`). Stripping `prefix` mirrors the CI `jq ltrimstr($ws)` step so
    /// the paths line up with the repo-relative paths git diffs report. Paths
    /// that do not start with `prefix` are left unchanged (already relative, or
    /// outside the tree). Leading `./` and `/` are also trimmed.
    pub fn strip_prefix(&mut self, prefix: &Path) {
        let prefix_str = prefix.to_string_lossy();
        let prefix_slash = format!("{}/", prefix_str.trim_end_matches('/'));
        let mut remapped: BTreeMap<String, FileCoverage> = BTreeMap::new();
        for (_, mut file) in std::mem::take(&mut self.files) {
            let normalized = normalize_path(&file.path, &prefix_slash);
            file.path.clone_from(&normalized);
            // Merge in case two source paths normalise to the same repo path.
            match remapped.get_mut(&normalized) {
                Some(existing) => {
                    for (line, hits) in std::mem::take(&mut file.lines) {
                        existing.record(line, hits);
                    }
                }
                None => {
                    remapped.insert(normalized, file);
                }
            }
        }
        self.files = remapped;
    }

    /// Drops every file whose path does not satisfy `keep`.
    ///
    /// Used to apply `--ignore-filename-regex`: because it is called *after*
    /// [`strip_prefix`](Self::strip_prefix), the predicate sees repo-relative
    /// paths — the same space git diffs report in — so head and baseline
    /// reports are filtered identically before any delta is computed.
    pub fn retain_paths<F>(&mut self, keep: F)
    where
        F: Fn(&str) -> bool,
    {
        self.files.retain(|path, _| keep(path));
    }
}

/// Strips `prefix_slash` (a trailing-slash directory prefix) from `path`, then
/// trims any leading `./` or `/`.
fn normalize_path(path: &str, prefix_slash: &str) -> String {
    let stripped = path.strip_prefix(prefix_slash).unwrap_or(path);
    stripped
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn record_takes_max_hits() {
        let mut f = FileCoverage::new("src/a.rs");
        f.record(1, 0);
        f.record(1, 3);
        f.record(1, 1);
        assert_eq!(f.lines.get(&1), Some(&3));
    }

    #[test]
    fn percent_counts_only_executable_lines() {
        let mut f = FileCoverage::new("src/a.rs");
        f.record(1, 1);
        f.record(2, 0);
        f.record(3, 5);
        // 2 of 3 executable lines covered.
        assert_eq!(f.total_lines(), 3);
        assert_eq!(f.covered_lines(), 2);
        assert!((f.percent().unwrap() - 66.666_666).abs() < 1e-3);
    }

    #[test]
    fn empty_file_has_no_percent() {
        let f = FileCoverage::new("src/empty.rs");
        assert_eq!(f.percent(), None);
    }

    #[test]
    fn hits_distinguishes_uncovered_from_non_executable() {
        let mut report = CoverageReport::new();
        let mut f = FileCoverage::new("src/a.rs");
        f.record(10, 0); // executable but uncovered
        report.insert(f);
        assert_eq!(report.hits("src/a.rs", 10), Some(0)); // uncovered
        assert_eq!(report.hits("src/a.rs", 11), None); // not instrumented
        assert_eq!(report.hits("src/missing.rs", 1), None);
    }

    #[test]
    fn insert_merges_duplicate_paths() {
        let mut report = CoverageReport::new();
        let mut a = FileCoverage::new("src/a.rs");
        a.record(1, 0);
        let mut b = FileCoverage::new("src/a.rs");
        b.record(1, 2);
        b.record(2, 1);
        report.insert(a);
        report.insert(b);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.hits("src/a.rs", 1), Some(2));
        assert_eq!(report.hits("src/a.rs", 2), Some(1));
    }

    #[test]
    fn project_percent_aggregates_files() {
        let mut report = CoverageReport::new();
        let mut a = FileCoverage::new("src/a.rs");
        a.record(1, 1);
        a.record(2, 1);
        let mut b = FileCoverage::new("src/b.rs");
        b.record(1, 0);
        b.record(2, 0);
        report.insert(a);
        report.insert(b);
        assert_eq!(report.total_lines(), 4);
        assert_eq!(report.covered_lines(), 2);
        assert_eq!(report.percent(), Some(50.0));
    }

    #[test]
    fn strip_prefix_makes_paths_repo_relative() {
        let mut report = CoverageReport::new();
        let mut f = FileCoverage::new("/home/runner/work/omni-dev/omni-dev/src/a.rs");
        f.record(1, 1);
        report.insert(f);
        report.strip_prefix(Path::new("/home/runner/work/omni-dev/omni-dev"));
        assert!(report.files.contains_key("src/a.rs"));
    }

    #[test]
    fn strip_prefix_merges_colliding_paths() {
        // Two distinct source paths that normalise to the same repo path.
        let mut report = CoverageReport::new();
        let mut a = FileCoverage::new("/root/src/a.rs");
        a.record(1, 0);
        let mut b = FileCoverage::new("/root/./src/a.rs");
        b.record(2, 1);
        report.insert(a);
        report.insert(b);
        assert_eq!(report.files.len(), 2);
        report.strip_prefix(Path::new("/root"));
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.hits("src/a.rs", 1), Some(0));
        assert_eq!(report.hits("src/a.rs", 2), Some(1));
    }

    #[test]
    fn strip_prefix_leaves_relative_paths() {
        let mut report = CoverageReport::new();
        let mut f = FileCoverage::new("./src/a.rs");
        f.record(1, 1);
        report.insert(f);
        report.strip_prefix(Path::new("/some/other/root"));
        assert!(report.files.contains_key("src/a.rs"));
    }

    #[test]
    fn retain_paths_drops_non_matching_files() {
        let mut report = CoverageReport::new();
        for path in ["src/a.rs", "src/gpu/mlx.rs", "src/b.rs"] {
            let mut f = FileCoverage::new(path);
            f.record(1, 1);
            report.insert(f);
        }
        report.retain_paths(|path| !path.contains("gpu/"));
        assert!(report.files.contains_key("src/a.rs"));
        assert!(report.files.contains_key("src/b.rs"));
        assert!(!report.files.contains_key("src/gpu/mlx.rs"));
    }
}
