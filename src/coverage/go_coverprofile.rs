//! Go `-coverprofile` parser (line coverage only).
//!
//! `go test -coverprofile` writes a `mode: set|count|atomic` header followed by
//! one record per instrumented *block*:
//!
//! ```text
//! mode: set
//! example.com/m/pkg/a.go:12.34,15.2 3 1
//! ```
//!
//! That is `file:startLine.startCol,endLine.endCol numStmts count`. A block is a
//! source range, not a line, so each block is expanded to every line it spans and
//! the line takes the block's count. Where blocks overlap the larger count wins,
//! which is the model's rule for any line several regions cover
//! ([`FileCoverage::record`]). A line shared by two blocks (`} else {`) therefore
//! reads covered when either ran: the blocks are sub-line spans and the model is
//! per line.
//!
//! A block with no statements (an empty function body) holds nothing to execute,
//! so it is not an executable line. The total is the number of distinct lines
//! some block spans, not the statement-weighted figure `go tool cover -func`
//! reports.
//!
//! The file names are Go import paths (`example.com/m/pkg/a.go`), not paths on
//! disk; [`module_path()`] reads the module an import path is rooted at so the
//! caller can make them repo-relative.
//!
//! Reference: <https://go.dev/blog/cover> and `cmd/cover`'s profile reader.

use anyhow::{bail, ensure, Context, Result};

use super::model::{CoverageReport, FileCoverage};

/// The `mode:` values `go test -covermode` writes.
const MODES: [&str; 3] = ["set", "count", "atomic"];

/// Most lines one block may span. Go source files do not approach this; the cap
/// keeps a corrupt range from allocating a line per number up to `u32::MAX`.
const MAX_BLOCK_LINES: u32 = 1_000_000;

/// Parses a Go coverprofile into a [`CoverageReport`] keyed by import path.
pub fn parse(content: &str) -> Result<CoverageReport> {
    let mut report = CoverageReport::new();
    let mut seen_mode = false;
    // A profile is written grouped by file, so one file's blocks are built up
    // in place and inserted once, rather than once per block.
    let mut current: Option<FileCoverage> = None;

    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lineno = index + 1;

        // Concatenated shards repeat the header, so it is accepted anywhere.
        if let Some(mode) = line.strip_prefix("mode:") {
            let mode = mode.trim();
            ensure!(
                MODES.contains(&mode),
                "go coverprofile line {lineno}: unknown mode `{mode}` (expected set, count or atomic)"
            );
            seen_mode = true;
            continue;
        }
        ensure!(
            seen_mode,
            "go coverprofile line {lineno}: expected a `mode:` header before the first block"
        );

        let block = parse_block(line)
            .with_context(|| format!("go coverprofile line {lineno}: malformed block"))?;
        if block.statements == 0 {
            continue;
        }
        if current.as_ref().is_none_or(|file| file.path != block.file) {
            if let Some(done) = current.replace(FileCoverage::new(block.file)) {
                report.insert(done);
            }
        }
        if let Some(file) = current.as_mut() {
            for number in block.start..=block.end {
                file.record(number, block.count);
            }
        }
    }
    if let Some(done) = current.take() {
        report.insert(done);
    }

    ensure!(seen_mode, "go coverprofile has no `mode:` header");
    Ok(report)
}

/// Returns the module path declared by a `go.mod`, or `None` when it has no
/// `module` directive.
///
/// The path may be bare, double-quoted or backtick-quoted, may be followed by a
/// `//` comment, and may sit on its own line inside the parenthesised
/// `module ( … )` form.
pub fn module_path(go_mod: &str) -> Option<String> {
    let mut lines = go_mod
        .lines()
        .map(|raw| raw.split("//").next().unwrap_or_default().trim());
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("module") else {
            continue;
        };
        // `modulefoo` is not a directive; the keyword needs a separator.
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let mut path = rest.trim();
        if path == "(" {
            path = lines
                .by_ref()
                .find(|line| !line.is_empty())
                .unwrap_or_default();
        }
        let path = path.trim_matches(['"', '`']);
        if !path.is_empty() && path != ")" {
            return Some(path.to_string());
        }
    }
    None
}

/// One parsed profile record.
struct Block<'a> {
    file: &'a str,
    start: u32,
    end: u32,
    statements: u64,
    count: u64,
}

/// Parses `file:startLine.startCol,endLine.endCol numStmts count`.
///
/// Fields are split from the right: the file name is whatever precedes the last
/// `:`, so a name that contains one (a Windows drive) is kept whole.
fn parse_block(line: &str) -> Result<Block<'_>> {
    let mut fields = line.rsplitn(3, char::is_whitespace);
    let count = fields.next().context("missing count")?;
    let statements = fields.next().context("missing statement count")?;
    let location = fields.next().context("missing location")?.trim_end();

    let (file, range) = location
        .rsplit_once(':')
        .context("location has no `file:range` separator")?;
    ensure!(!file.is_empty(), "location has an empty file name");
    let (start, end) = range
        .split_once(',')
        .context("range has no `start,end` separator")?;
    let start = line_of(start).with_context(|| format!("invalid start `{start}`"))?;
    let end = line_of(end).with_context(|| format!("invalid end `{end}`"))?;
    if end < start {
        bail!("range ends on line {end}, before it starts on line {start}");
    }
    ensure!(
        end - start < MAX_BLOCK_LINES,
        "range spans more than {MAX_BLOCK_LINES} lines"
    );

    Ok(Block {
        file,
        start,
        end,
        statements: statements
            .parse()
            .with_context(|| format!("invalid statement count `{statements}`"))?,
        count: count
            .parse()
            .with_context(|| format!("invalid count `{count}`"))?,
    })
}

/// The line of a `line.column` position.
fn line_of(position: &str) -> Result<u32> {
    let (line, column) = position
        .split_once('.')
        .context("position is not `line.column`")?;
    column
        .parse::<u32>()
        .with_context(|| format!("invalid column `{column}`"))?;
    line.parse()
        .with_context(|| format!("invalid line `{line}`"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_block_expands_to_every_line_it_spans() {
        let report = parse("mode: set\nm/a.go:3.10,5.2 2 1\n").unwrap();
        for line in 3..=5 {
            assert_eq!(report.hits("m/a.go", line), Some(1), "line {line}");
        }
        assert_eq!(report.hits("m/a.go", 2), None);
        assert_eq!(report.hits("m/a.go", 6), None);
        assert_eq!(report.total_lines(), 3);
    }

    #[test]
    fn set_mode_records_zero_and_one() {
        let report = parse("mode: set\nm/a.go:1.1,1.9 1 1\nm/a.go:2.1,2.9 1 0\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), Some(1));
        assert_eq!(report.hits("m/a.go", 2), Some(0));
    }

    #[test]
    fn count_mode_keeps_the_count() {
        let report = parse("mode: count\nm/a.go:1.1,1.9 1 42\nm/a.go:2.1,2.9 1 0\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), Some(42));
        assert_eq!(report.hits("m/a.go", 2), Some(0));
    }

    #[test]
    fn atomic_mode_keeps_a_count_past_u32() {
        let report = parse("mode: atomic\nm/a.go:1.1,1.9 1 5000000000\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), Some(5_000_000_000));
    }

    #[test]
    fn overlapping_blocks_take_the_larger_count_in_either_order() {
        let covered_first = parse("mode: count\nm/a.go:1.1,3.2 2 7\nm/a.go:3.5,3.9 1 0\n").unwrap();
        let covered_last = parse("mode: count\nm/a.go:3.5,3.9 1 0\nm/a.go:1.1,3.2 2 7\n").unwrap();
        assert_eq!(covered_first, covered_last);
        assert_eq!(covered_first.hits("m/a.go", 3), Some(7));
        assert_eq!(covered_first.total_lines(), 3);
    }

    #[test]
    fn a_line_shared_by_an_uncovered_and_a_covered_block_reads_covered() {
        // `} else {` on line 3: the `if` arm never ran, the `else` arm did.
        let report = parse("mode: set\nm/a.go:2.5,3.3 1 0\nm/a.go:3.9,4.3 1 1\n").unwrap();
        assert_eq!(report.hits("m/a.go", 2), Some(0));
        assert_eq!(report.hits("m/a.go", 3), Some(1));
        assert_eq!(report.hits("m/a.go", 4), Some(1));
    }

    #[test]
    fn a_block_with_no_statements_is_not_executable() {
        let report = parse("mode: set\nm/a.go:1.10,1.12 0 0\nm/a.go:2.1,2.9 1 1\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), None);
        assert_eq!(report.total_lines(), 1);
    }

    #[test]
    fn a_file_of_only_empty_blocks_is_absent() {
        let report = parse("mode: set\nm/a.go:1.10,1.12 0 0\n").unwrap();
        assert!(report.files.is_empty());
    }

    #[test]
    fn a_file_whose_blocks_are_not_contiguous_is_still_one_file() {
        let report =
            parse("mode: set\nm/a.go:1.1,1.9 1 1\nm/b.go:1.1,1.9 1 1\nm/a.go:2.1,2.9 1 0\n")
                .unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.hits("m/a.go", 1), Some(1));
        assert_eq!(report.hits("m/a.go", 2), Some(0));
    }

    #[test]
    fn files_are_kept_apart() {
        let report = parse("mode: set\nm/a.go:1.1,1.9 1 1\nm/b/b.go:1.1,1.9 1 0\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), Some(1));
        assert_eq!(report.hits("m/b/b.go", 1), Some(0));
    }

    #[test]
    fn a_header_only_profile_is_an_empty_report() {
        assert!(parse("mode: atomic\n").unwrap().files.is_empty());
    }

    #[test]
    fn a_repeated_header_is_accepted_as_a_shard_separator() {
        let report =
            parse("mode: set\nm/a.go:1.1,1.9 1 0\nmode: set\nm/a.go:1.1,1.9 1 1\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), Some(1));
    }

    #[test]
    fn shards_in_different_modes_are_still_merged_by_max() {
        let report =
            parse("mode: set\nm/a.go:1.1,1.9 1 1\nmode: count\nm/a.go:1.1,1.9 1 9\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), Some(9));
    }

    #[test]
    fn crlf_and_blank_lines_are_tolerated() {
        let report = parse("mode: set\r\n\r\nm/a.go:1.1,1.9 1 1\r\n").unwrap();
        assert_eq!(report.hits("m/a.go", 1), Some(1));
    }

    #[test]
    fn a_file_name_containing_a_colon_is_kept_whole() {
        let report = parse("mode: set\nC:/work/m/a.go:4.1,4.9 1 1\n").unwrap();
        assert_eq!(report.hits("C:/work/m/a.go", 4), Some(1));
    }

    #[test]
    fn a_file_name_containing_a_space_is_kept_whole() {
        let report = parse("mode: set\nm/my pkg/a.go:4.1,4.9 1 1\n").unwrap();
        assert_eq!(report.hits("m/my pkg/a.go", 4), Some(1));
    }

    #[test]
    fn a_missing_header_is_an_error() {
        let message = format!("{:#}", parse("m/a.go:1.1,1.9 1 1\n").unwrap_err());
        assert!(message.contains("mode:"), "{message}");
        assert!(parse("").is_err());
    }

    #[test]
    fn an_unknown_mode_is_an_error() {
        let message = format!("{:#}", parse("mode: sometimes\n").unwrap_err());
        assert!(message.contains("sometimes"), "{message}");
    }

    #[test]
    fn malformed_blocks_are_errors_naming_the_line() {
        for bad in [
            "m/a.go",
            "m/a.go:1.1,1.9 1",
            "m/a.go 1.1,1.9 1 1",
            "m/a.go:1.1 1 1",
            "m/a.go:1,1 1 1",
            "m/a.go:x.1,1.9 1 1",
            "m/a.go:1.x,1.9 1 1",
            "m/a.go:1.1,1.9 x 1",
            "m/a.go:1.1,1.9 1 -1",
            "m/a.go:5.1,4.9 1 1",
            ":1.1,1.9 1 1",
        ] {
            let message = format!("{:#}", parse(&format!("mode: set\n{bad}\n")).unwrap_err());
            assert!(message.contains("line 2"), "{bad}: {message}");
        }
    }

    #[test]
    fn a_block_spanning_an_absurd_range_is_refused() {
        let message = format!(
            "{:#}",
            parse("mode: set\nm/a.go:1.1,4000000000.1 1 1\n").unwrap_err()
        );
        assert!(message.contains("spans more than"), "{message}");
    }

    #[test]
    fn module_path_reads_the_module_directive() {
        assert_eq!(
            module_path("module example.com/m\n\ngo 1.22\n").as_deref(),
            Some("example.com/m")
        );
    }

    #[test]
    fn module_path_handles_quotes_comments_and_indentation() {
        assert_eq!(
            module_path("// header\n  module \"example.com/m\" // note\n").as_deref(),
            Some("example.com/m")
        );
        assert_eq!(
            module_path("module `example.com/m`\n").as_deref(),
            Some("example.com/m")
        );
    }

    #[test]
    fn module_path_reads_the_parenthesised_form() {
        assert_eq!(
            module_path("module (\n\t// the path\n\t\"example.com/m\"\n)\n").as_deref(),
            Some("example.com/m")
        );
        assert_eq!(module_path("module (\n)\n"), None);
        assert_eq!(module_path("module (\n"), None);
    }

    #[test]
    fn module_path_ignores_other_directives_and_a_missing_module() {
        assert_eq!(module_path("go 1.22\nrequire x v1\n"), None);
        assert_eq!(module_path("modulefoo example.com/m\n"), None);
        assert_eq!(module_path("module\n"), None);
        assert_eq!(module_path(""), None);
    }
}
