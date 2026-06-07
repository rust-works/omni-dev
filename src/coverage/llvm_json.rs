//! llvm-cov JSON export parser (`cargo llvm-cov report --json`).
//!
//! The export records per-file *segments* rather than per-line counts. Each
//! segment is `[line, col, count, has_count, is_region_entry, is_gap_region]`
//! and marks a point where the active region count changes. Reducing segments
//! to per-line hit counts reproduces llvm's own `LineCoverageStats` algorithm:
//! for each source line, take the region active at the line's start (the
//! "wrapped" segment) together with any region-entry segments on the line, and
//! the line's count is the maximum among them. A line is instrumented ("mapped")
//! when a counted region is active over it and it is not the start of a skipped
//! region.

use anyhow::{Context, Result};
use serde_json::Value;

use super::model::{CoverageReport, FileCoverage};

/// One coverage segment from the llvm-cov JSON export.
#[derive(Debug, Clone, Copy)]
struct Segment {
    line: u32,
    count: u64,
    has_count: bool,
    is_region_entry: bool,
    is_gap: bool,
}

/// Parses llvm-cov JSON export text into a [`CoverageReport`].
pub fn parse(content: &str) -> Result<CoverageReport> {
    let root: Value = serde_json::from_str(content).context("invalid llvm-cov JSON")?;
    let data = root
        .get("data")
        .and_then(Value::as_array)
        .context("llvm-cov JSON: missing `data` array")?;

    let mut report = CoverageReport::new();
    for export in data {
        let Some(files) = export.get("files").and_then(Value::as_array) else {
            continue;
        };
        for file in files {
            let Some(filename) = file.get("filename").and_then(Value::as_str) else {
                continue;
            };
            let segments = file
                .get("segments")
                .and_then(Value::as_array)
                .map(|s| parse_segments(s))
                .unwrap_or_default();
            let coverage = reduce_segments(filename, &segments);
            if !coverage.lines.is_empty() {
                report.insert(coverage);
            }
        }
    }
    Ok(report)
}

/// Parses the raw segment arrays into [`Segment`] values, skipping malformed rows.
fn parse_segments(raw: &[Value]) -> Vec<Segment> {
    raw.iter()
        .filter_map(|seg| {
            let arr = seg.as_array()?;
            Some(Segment {
                line: arr.first()?.as_u64()? as u32,
                count: count_of(arr.get(2)),
                has_count: arr.get(3).and_then(Value::as_bool).unwrap_or(false),
                is_region_entry: arr.get(4).and_then(Value::as_bool).unwrap_or(false),
                is_gap: arr.get(5).and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

/// Reads a segment count, tolerating both integer and float JSON encodings.
fn count_of(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// Reduces a file's segments to per-line hit counts.
fn reduce_segments(filename: &str, segments: &[Segment]) -> FileCoverage {
    let mut file = FileCoverage::new(filename);
    if segments.is_empty() {
        return file;
    }

    let max_line = segments.iter().map(|s| s.line).max().unwrap_or(0);
    let min_line = segments.iter().map(|s| s.line).min().unwrap_or(0);

    // The region active at the start of the current line (last segment on an
    // earlier line). Updated to the last segment of each line as we advance.
    let mut wrapped: Option<Segment> = None;
    let mut seg_idx = 0usize;

    for line in min_line..=max_line {
        // Collect segments that fall on this line (segments are source-ordered).
        let start = seg_idx;
        while seg_idx < segments.len() && segments[seg_idx].line == line {
            seg_idx += 1;
        }
        let line_segments = &segments[start..seg_idx];

        if let Some(count) = line_stat(wrapped.as_ref(), line_segments) {
            file.record(line, count);
        }

        if let Some(last) = line_segments.last() {
            wrapped = Some(*last);
        }
    }

    file
}

/// Computes the execution count for one line, or `None` when the line is not
/// instrumented. Mirrors llvm's `LineCoverageStats`.
fn line_stat(wrapped: Option<&Segment>, line_segments: &[Segment]) -> Option<u64> {
    let is_start_of_region = |s: &Segment| !s.is_gap && s.has_count && s.is_region_entry;

    let start_of_skipped = line_segments
        .first()
        .is_some_and(|s| !s.has_count && s.is_region_entry);

    let min_region_count = line_segments
        .iter()
        .filter(|s| is_start_of_region(s))
        .count();

    let wrapped_has_count = wrapped.is_some_and(|w| w.has_count);
    let mapped = !start_of_skipped && (wrapped_has_count || min_region_count > 0);
    if !mapped {
        return None;
    }

    let mut count = 0u64;
    if let Some(w) = wrapped {
        if w.has_count {
            count = w.count;
        }
    }
    for s in line_segments {
        if is_start_of_region(s) {
            count = count.max(s.count);
        }
    }
    Some(count)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn export(files: Value) -> String {
        serde_json::to_string(&serde_json::json!({
            "data": [{ "files": files }],
            "type": "llvm.coverage.json.export",
            "version": "2.0.1"
        }))
        .unwrap()
    }

    #[test]
    fn single_region_spans_multiple_lines() {
        // Region entry at line 1 (count 3), exit at line 4 (count 0, end of fn).
        let json = export(serde_json::json!([{
            "filename": "src/a.rs",
            "segments": [
                [1, 1, 3, true, true, false],
                [4, 2, 0, false, false, false]
            ]
        }]));
        let report = parse(&json).unwrap();
        // Lines 1..=3 are wrapped by the count-3 region → covered.
        assert_eq!(report.hits("src/a.rs", 1), Some(3));
        assert_eq!(report.hits("src/a.rs", 2), Some(3));
        assert_eq!(report.hits("src/a.rs", 3), Some(3));
    }

    #[test]
    fn uncovered_region() {
        let json = export(serde_json::json!([{
            "filename": "src/a.rs",
            "segments": [
                [10, 1, 0, true, true, false],
                [12, 1, 0, false, false, false]
            ]
        }]));
        let report = parse(&json).unwrap();
        // Lines 10 and 11 are wrapped by the count-0 region; line 12 is the
        // region-closing line (llvm reports it as instrumented, count 0).
        assert_eq!(report.hits("src/a.rs", 10), Some(0));
        assert_eq!(report.hits("src/a.rs", 11), Some(0));
        assert_eq!(report.hits("src/a.rs", 12), Some(0));
        assert_eq!(report.covered_lines(), 0);
        assert_eq!(report.total_lines(), 3);
    }

    #[test]
    fn gap_region_is_not_a_region_start() {
        // A gap region (is_gap=true) should not by itself mark a line covered;
        // it only contributes through the wrapped count.
        let json = export(serde_json::json!([{
            "filename": "src/a.rs",
            "segments": [
                [1, 1, 5, true, true, false],
                [2, 1, 0, true, true, true],
                [3, 1, 5, true, true, false],
                [4, 1, 0, false, false, false]
            ]
        }]));
        let report = parse(&json).unwrap();
        assert_eq!(report.hits("src/a.rs", 1), Some(5));
        assert_eq!(report.hits("src/a.rs", 3), Some(5));
    }

    #[test]
    fn nested_region_takes_max() {
        // Outer region count 2 from line 1; inner region count 7 starts on line 2.
        let json = export(serde_json::json!([{
            "filename": "src/a.rs",
            "segments": [
                [1, 1, 2, true, true, false],
                [2, 5, 7, true, true, false],
                [2, 20, 2, true, false, false],
                [3, 1, 0, false, false, false]
            ]
        }]));
        let report = parse(&json).unwrap();
        assert_eq!(report.hits("src/a.rs", 1), Some(2));
        // Line 2 has the inner region entry (7) → max(2, 7) = 7.
        assert_eq!(report.hits("src/a.rs", 2), Some(7));
    }

    #[test]
    fn empty_segments_yields_no_lines() {
        let json = export(serde_json::json!([{ "filename": "src/a.rs", "segments": [] }]));
        let report = parse(&json).unwrap();
        assert!(report.files.is_empty());
    }
}
