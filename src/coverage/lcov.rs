//! lcov trace-file parser and writer (line coverage only).
//!
//! lcov records one source file per `SF:`…`end_of_record` block. Within a block,
//! `DA:<line>,<hits>[,<checksum>]` gives the hit count for an instrumented line.
//! Branch (`BRDA`) and function (`FN*`) records are ignored — v1 is scoped to
//! line coverage to match the existing coverage comment.
//!
//! Reference: <https://manpages.debian.org/unstable/lcov/geninfo.1.en.html>

use std::fmt::Write as _;

use anyhow::{bail, Context, Result};

use super::model::{CoverageReport, FileCoverage};

/// Parses lcov trace text into a [`CoverageReport`].
pub fn parse(content: &str) -> Result<CoverageReport> {
    let mut report = CoverageReport::new();
    let mut current: Option<FileCoverage> = None;

    for (lineno, raw) in content.lines().enumerate() {
        let mut line = raw.trim();

        // `cargo llvm-cov` writes no newline after its final `end_of_record`, so
        // two reports concatenated with `cat` put the next file's `SF:` on the
        // same line. What follows the terminator is an ordinary record, not
        // noise: dropping it loses a whole file's coverage without an error.
        while let Some(rest) = line.strip_prefix("end_of_record") {
            if let Some(file) = current.take() {
                report.insert(file);
            }
            line = rest.trim_start();
        }
        if line.is_empty() {
            continue;
        }

        if let Some(path) = line.strip_prefix("SF:") {
            // A block that never saw its `end_of_record` (a truncated report
            // followed by another) is kept, not silently replaced.
            if let Some(unfinished) = current.replace(FileCoverage::new(path.trim())) {
                report.insert(unfinished);
            }
        } else if let Some(rest) = line.strip_prefix("DA:") {
            let file = current.as_mut().with_context(|| {
                format!("lcov line {}: DA record outside of an SF block", lineno + 1)
            })?;
            let (number, hits) = parse_da(rest)
                .with_context(|| format!("lcov line {}: malformed DA record", lineno + 1))?;
            file.record(number, hits);
        }
        // All other records (TN, BRDA, FN, FNDA, LF, LH, …) are ignored.
    }

    // Tolerate a trailing block with no explicit end_of_record.
    if let Some(file) = current.take() {
        report.insert(file);
    }

    Ok(report)
}

/// Renders `report` as lcov trace text.
///
/// The output is a function of the report alone: files come out in path order
/// and each file's `DA` records in line order, so two reports that compare equal
/// write byte-identical text, whatever order their inputs were merged in. Every
/// record ends in a newline — the last `end_of_record` included, which
/// `cargo llvm-cov` omits and which makes its files unsafe to join with `cat`.
///
/// Only what the line model holds is written: `TN`, `SF`, `DA`, `LF`, `LH` and
/// `end_of_record`. Function (`FN*`) and branch (`BRDA`) records are not part of
/// the model and are dropped. `LF`/`LH` count the `DA` records written, so a
/// file is consistent with itself; `llvm-cov` writes them from its own summary
/// instead, which counts some lines more than once (#2131).
///
/// A file with no executable lines is omitted: it has no coverage to report, and
/// [`CoverageReport::retain_lines`] already treats one as absent.
///
/// # Errors
///
/// Fails when a path contains a line break, which lcov has no way to carry: the
/// file would parse back as different records.
pub fn write(report: &CoverageReport) -> Result<String> {
    let mut out = String::new();
    for (path, file) in &report.files {
        if file.total_lines() == 0 {
            continue;
        }
        if path.contains(['\n', '\r']) {
            bail!("cannot write {path:?} to lcov: the path contains a line break");
        }
        // Writing to a `String` cannot fail.
        let _ = writeln!(out, "TN:\nSF:{path}");
        for (line, hits) in &file.lines {
            let _ = writeln!(out, "DA:{line},{hits}");
        }
        let _ = writeln!(
            out,
            "LF:{}\nLH:{}\nend_of_record",
            file.total_lines(),
            file.covered_lines()
        );
    }
    Ok(out)
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

    /// Shard files joined with a newline parse as the same union
    /// `CoverageReport::merge` builds: the docs tell a caller that needs a single
    /// file (a baseline) that this is equivalent, so that has to stay true.
    ///
    /// The shards end like real `cargo llvm-cov` output — **no trailing newline**
    /// after the last `end_of_record`, which is why the join needs its own.
    #[test]
    fn newline_joined_shards_parse_as_their_merge() {
        let shard_a = "TN:\nSF:src/a.rs\nDA:1,3\nDA:2,0\nend_of_record";
        let shard_b = "TN:\nSF:src/a.rs\nDA:1,0\nDA:2,2\nDA:3,0\nend_of_record\n\
                       SF:src/b.rs\nDA:1,1\nend_of_record";

        let mut merged = parse(shard_a).unwrap();
        merged.merge(parse(shard_b).unwrap());
        let concatenated = parse(&format!("{shard_a}\n{shard_b}")).unwrap();

        assert_eq!(concatenated, merged);
        assert_eq!(concatenated.hits("src/a.rs", 1), Some(3));
        assert_eq!(concatenated.hits("src/a.rs", 2), Some(2));
        assert_eq!(concatenated.hits("src/a.rs", 3), Some(0));
    }

    /// Plain `cat` of real `cargo llvm-cov` shards (no trailing newline) glues
    /// `end_of_record` onto the next `SF:`. Both files must survive with their
    /// own lines: before this was handled the last file of each shard vanished
    /// and the next file's lines landed on it, understating coverage silently.
    #[test]
    fn a_glued_end_of_record_does_not_lose_the_next_file() {
        let lcov = "SF:src/a.rs\nDA:1,1\nend_of_recordSF:src/b.rs\nDA:1,0\nDA:2,3\nend_of_record";
        let report = parse(lcov).unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.hits("src/a.rs", 1), Some(1));
        assert_eq!(report.hits("src/a.rs", 2), None);
        assert_eq!(report.hits("src/b.rs", 1), Some(0));
        assert_eq!(report.hits("src/b.rs", 2), Some(3));
    }

    #[test]
    fn plainly_concatenated_shards_parse_as_their_merge() {
        let shard_a =
            "TN:\nSF:src/a.rs\nDA:1,3\nDA:2,0\nend_of_record\nSF:src/c.rs\nDA:1,0\nend_of_record";
        let shard_b =
            "TN:\nSF:src/a.rs\nDA:1,0\nDA:2,2\nend_of_record\nSF:src/c.rs\nDA:1,1\nend_of_record";

        let mut merged = parse(shard_a).unwrap();
        merged.merge(parse(shard_b).unwrap());
        // No separator at all: exactly what `cat shard-*.lcov` produces.
        let catted = parse(&format!("{shard_a}{shard_b}")).unwrap();

        assert_eq!(catted, merged);
        assert_eq!(catted.hits("src/c.rs", 1), Some(1));
    }

    #[test]
    fn a_block_without_end_of_record_is_kept_when_another_file_follows() {
        let lcov = "SF:src/a.rs\nDA:1,1\nSF:src/b.rs\nDA:1,0\nend_of_record\n";
        let report = parse(lcov).unwrap();
        assert_eq!(report.hits("src/a.rs", 1), Some(1));
        assert_eq!(report.hits("src/b.rs", 1), Some(0));
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

    #[test]
    fn skips_blank_lines() {
        let lcov = "\nSF:a.rs\n\nDA:1,1\n\nend_of_record\n\n";
        let report = parse(lcov).unwrap();
        assert_eq!(report.hits("a.rs", 1), Some(1));
    }

    #[test]
    fn negative_hit_count_saturates_to_zero() {
        let report = parse("SF:a.rs\nDA:1,-5\nend_of_record\n").unwrap();
        assert_eq!(report.hits("a.rs", 1), Some(0));
    }

    // ── write (#2118) ────────────────────────────────────────────────────

    fn report(files: &[(&str, &[(u32, u64)])]) -> CoverageReport {
        let mut report = CoverageReport::new();
        for (path, lines) in files {
            let mut file = FileCoverage::new(*path);
            for &(line, hits) in *lines {
                file.record(line, hits);
            }
            report.insert(file);
        }
        report
    }

    #[test]
    fn write_emits_one_sorted_record_per_file() {
        // Inserted out of order on purpose: the output must not depend on it.
        let report = report(&[("src/b.rs", &[(2, 0), (1, 3)]), ("src/a.rs", &[(5, 1)])]);
        assert_eq!(
            write(&report).unwrap(),
            "TN:\nSF:src/a.rs\nDA:5,1\nLF:1\nLH:1\nend_of_record\n\
             TN:\nSF:src/b.rs\nDA:1,3\nDA:2,0\nLF:2\nLH:1\nend_of_record\n"
        );
    }

    /// `cargo llvm-cov` omits the newline after its last `end_of_record`, so a
    /// file that stops there cannot be concatenated safely. Ours must not.
    #[test]
    fn write_ends_every_record_with_a_newline() {
        let text = write(&report(&[("src/a.rs", &[(1, 1)])])).unwrap();
        assert!(text.ends_with("end_of_record\n"), "{text:?}");
    }

    #[test]
    fn write_of_an_empty_report_is_empty() {
        assert_eq!(write(&CoverageReport::new()).unwrap(), "");
    }

    #[test]
    fn write_omits_a_file_with_no_executable_lines() {
        let report = report(&[("src/empty.rs", &[]), ("src/a.rs", &[(1, 0)])]);
        let text = write(&report).unwrap();
        assert!(!text.contains("empty.rs"), "{text}");
        assert!(text.contains("SF:src/a.rs"), "{text}");
    }

    #[test]
    fn write_reads_back_as_the_same_report() {
        let original = report(&[
            ("src/a.rs", &[(1, 0), (2, 7), (40, 1)]),
            ("/abs/b.rs", &[(3, 2)]),
        ]);
        assert_eq!(parse(&write(&original).unwrap()).unwrap(), original);
    }

    /// `LF`/`LH` are written from the `DA` records, so the totals agree with the
    /// lines listed — unlike `llvm-cov`'s own lcov (#2131).
    #[test]
    fn write_counts_lf_and_lh_from_its_own_da_records() {
        let text = write(&report(&[("a.rs", &[(1, 0), (2, 5), (3, 9)])])).unwrap();
        assert!(text.contains("\nLF:3\nLH:2\n"), "{text}");
    }

    #[test]
    fn write_is_independent_of_the_order_shards_were_merged_in() {
        let shards = [
            "SF:src/a.rs\nDA:1,3\nDA:2,0\nend_of_record\nSF:src/c.rs\nDA:1,0\nend_of_record",
            "SF:src/a.rs\nDA:1,0\nDA:2,2\nend_of_record\nSF:src/b.rs\nDA:9,1\nend_of_record",
            "SF:src/c.rs\nDA:1,4\nDA:7,0\nend_of_record",
        ];
        let merged_in = |order: [usize; 3]| {
            let mut merged = CoverageReport::new();
            for index in order {
                merged.merge(parse(shards[index]).unwrap());
            }
            write(&merged).unwrap()
        };
        let reference = merged_in([0, 1, 2]);
        for order in [[0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            assert_eq!(merged_in(order), reference, "{order:?}");
        }
    }

    #[test]
    fn write_rejects_a_path_with_a_line_break() {
        for path in ["src/a\nSF:evil.rs", "src/a\rb.rs"] {
            let message = write(&report(&[(path, &[(1, 1)])]))
                .unwrap_err()
                .to_string();
            assert!(message.contains("line break"), "{message}");
        }
    }
}
