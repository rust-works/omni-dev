//! lcov trace-file parser (line coverage only).
//!
//! lcov records one source file per `SF:`…`end_of_record` block. Within a block,
//! `DA:<line>,<hits>[,<checksum>]` gives the hit count for an instrumented line.
//! Branch (`BRDA`) and function (`FN*`) records are ignored — v1 is scoped to
//! line coverage to match the existing coverage comment.
//!
//! Reference: <https://manpages.debian.org/unstable/lcov/geninfo.1.en.html>

use anyhow::{Context, Result};

use super::model::{CoverageReport, FileCoverage};

/// Parses lcov trace text into a [`CoverageReport`].
pub fn parse(content: &str) -> Result<CoverageReport> {
    let mut report = CoverageReport::new();
    let mut current: Option<FileCoverage> = None;

    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(path) = line.strip_prefix("SF:") {
            current = Some(FileCoverage::new(path.trim()));
        } else if let Some(rest) = line.strip_prefix("DA:") {
            let file = current.as_mut().with_context(|| {
                format!("lcov line {}: DA record outside of an SF block", lineno + 1)
            })?;
            let (number, hits) = parse_da(rest)
                .with_context(|| format!("lcov line {}: malformed DA record", lineno + 1))?;
            file.record(number, hits);
        } else if line == "end_of_record" {
            if let Some(file) = current.take() {
                report.insert(file);
            }
        }
        // All other records (TN, BRDA, FN, FNDA, LF, LH, …) are ignored.
    }

    // Tolerate a trailing block with no explicit end_of_record.
    if let Some(file) = current.take() {
        report.insert(file);
    }

    Ok(report)
}

/// Parses the payload of a `DA:` record (`<line>,<hits>[,<checksum>]`).
fn parse_da(rest: &str) -> Result<(u32, u64)> {
    let mut parts = rest.split(',');
    let number: u32 = parts
        .next()
        .context("missing line number")?
        .trim()
        .parse()
        .context("invalid line number")?;
    // Hit counts can overflow i64 in pathological cases; lcov emits them as
    // decimal integers. Saturate negative or out-of-range values to 0.
    let hits: u64 = parts
        .next()
        .context("missing hit count")?
        .trim()
        .parse()
        .unwrap_or(0);
    Ok((number, hits))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_file() {
        let lcov = "\
TN:
SF:/repo/src/a.rs
DA:1,5
DA:2,0
DA:3,1
LF:3
LH:2
end_of_record
";
        let report = parse(lcov).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.hits("/repo/src/a.rs", 1), Some(5));
        assert_eq!(report.hits("/repo/src/a.rs", 2), Some(0));
        assert_eq!(report.hits("/repo/src/a.rs", 3), Some(1));
        assert_eq!(report.total_lines(), 3);
        assert_eq!(report.covered_lines(), 2);
    }

    #[test]
    fn parses_multiple_files() {
        let lcov = "\
SF:src/a.rs
DA:1,1
end_of_record
SF:src/b.rs
DA:1,0
DA:2,2
end_of_record
";
        let report = parse(lcov).unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.hits("src/a.rs", 1), Some(1));
        assert_eq!(report.hits("src/b.rs", 2), Some(2));
    }

    #[test]
    fn ignores_branch_and_function_records() {
        let lcov = "\
SF:src/a.rs
FN:1,foo
FNDA:3,foo
DA:1,3
BRDA:1,0,0,1
end_of_record
";
        let report = parse(lcov).unwrap();
        let f = &report.files["src/a.rs"];
        assert_eq!(f.lines.len(), 1);
        assert_eq!(f.lines.get(&1), Some(&3));
    }

    #[test]
    fn da_with_checksum() {
        let lcov = "SF:src/a.rs\nDA:1,4,abcdef\nend_of_record\n";
        let report = parse(lcov).unwrap();
        assert_eq!(report.hits("src/a.rs", 1), Some(4));
    }

    #[test]
    fn tolerates_missing_end_of_record() {
        let lcov = "SF:src/a.rs\nDA:1,1\n";
        let report = parse(lcov).unwrap();
        assert_eq!(report.hits("src/a.rs", 1), Some(1));
    }

    #[test]
    fn da_outside_sf_is_error() {
        let lcov = "DA:1,1\n";
        assert!(parse(lcov).is_err());
    }
}
