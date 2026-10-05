//! Coverage report format detection and parse dispatch.

use std::fmt;

use anyhow::{Context, Result};

use super::model::CoverageReport;
use super::{cobertura, go_coverprofile, lcov, llvm_json};

/// A supported per-line coverage report format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// lcov trace file (`DA`/`SF`/`end_of_record`).
    Lcov,
    /// llvm-cov JSON export (`cargo llvm-cov report --json`).
    LlvmCovJson,
    /// Cobertura XML.
    Cobertura,
    /// Go `go test -coverprofile` output (`mode: set|count|atomic` header).
    GoCoverprofile,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Lcov => "lcov",
            Self::LlvmCovJson => "llvm-cov-json",
            Self::Cobertura => "cobertura",
            Self::GoCoverprofile => "go-coverprofile",
        };
        f.write_str(name)
    }
}

impl Format {
    /// Detects the format from report `content`.
    ///
    /// Detection is by leading non-whitespace character/token: XML opens with
    /// `<`, JSON with `{`, lcov with a record keyword (`TN:`/`SF:`), and a Go
    /// coverprofile with its `mode:` header. The header is matched on `mode:`
    /// alone, so a profile with a mode Go does not write fails in the parser,
    /// naming it, instead of reading as an unrecognised format.
    pub fn detect(content: &str) -> Result<Self> {
        let trimmed = content.trim_start();
        let first = trimmed
            .chars()
            .next()
            .context("coverage report is empty; cannot detect format")?;
        match first {
            '<' => Ok(Self::Cobertura),
            '{' | '[' => Ok(Self::LlvmCovJson),
            _ if trimmed.starts_with("TN:")
                || trimmed.starts_with("SF:")
                || trimmed.starts_with("DA:") =>
            {
                Ok(Self::Lcov)
            }
            _ if trimmed.starts_with("mode:") => Ok(Self::GoCoverprofile),
            _ => anyhow::bail!(
                "could not auto-detect coverage report format; pass an explicit --report-format \
                 (lcov, llvm-cov-json, cobertura, or go-coverprofile)"
            ),
        }
    }

    /// Parses `content` according to this format.
    pub fn parse(self, content: &str) -> Result<CoverageReport> {
        match self {
            Self::Lcov => lcov::parse(content),
            Self::LlvmCovJson => llvm_json::parse(content),
            Self::Cobertura => cobertura::parse(content),
            Self::GoCoverprofile => go_coverprofile::parse(content),
        }
    }
}

/// Resolves `format`, auto-detecting from `content` when it is `None`.
pub fn resolve(content: &str, format: Option<Format>) -> Result<Format> {
    match format {
        Some(f) => Ok(f),
        None => Format::detect(content).context("coverage report format auto-detection failed"),
    }
}

/// Parses `content` using `format`, auto-detecting when `format` is `None`.
pub fn parse(content: &str, format: Option<Format>) -> Result<CoverageReport> {
    let format = resolve(content, format)?;
    format
        .parse(content)
        .with_context(|| format!("failed to parse {format} coverage report"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn detects_lcov() {
        assert_eq!(
            Format::detect("SF:src/a.rs\nDA:1,1\n").unwrap(),
            Format::Lcov
        );
        assert_eq!(Format::detect("TN:\nSF:x\n").unwrap(), Format::Lcov);
    }

    #[test]
    fn detects_json() {
        assert_eq!(
            Format::detect("  {\"data\":[]}").unwrap(),
            Format::LlvmCovJson
        );
    }

    #[test]
    fn detects_cobertura() {
        assert_eq!(
            Format::detect("<?xml version=\"1.0\"?><coverage/>").unwrap(),
            Format::Cobertura
        );
    }

    #[test]
    fn detects_go_coverprofile_in_every_mode() {
        for mode in ["set", "count", "atomic"] {
            let content = format!("mode: {mode}\nm/a.go:1.1,1.9 1 1\n");
            assert_eq!(Format::detect(&content).unwrap(), Format::GoCoverprofile);
        }
        assert_eq!(
            Format::detect("\n  mode: set\n").unwrap(),
            Format::GoCoverprofile
        );
    }

    #[test]
    fn an_unknown_go_mode_is_detected_so_the_parser_can_name_it() {
        assert_eq!(
            Format::detect("mode: sometimes\n").unwrap(),
            Format::GoCoverprofile
        );
        let message = format!("{:#}", parse("mode: sometimes\n", None).unwrap_err());
        assert!(message.contains("sometimes"), "{message}");
    }

    #[test]
    fn unknown_format_errors() {
        assert!(Format::detect("hello world").is_err());
        assert!(Format::detect("").is_err());
    }

    #[test]
    fn parse_dispatches_by_detection() {
        let report = parse("SF:src/a.rs\nDA:1,2\nend_of_record\n", None).unwrap();
        assert_eq!(report.hits("src/a.rs", 1), Some(2));
    }

    #[test]
    fn display_names() {
        assert_eq!(Format::Lcov.to_string(), "lcov");
        assert_eq!(Format::LlvmCovJson.to_string(), "llvm-cov-json");
        assert_eq!(Format::Cobertura.to_string(), "cobertura");
        assert_eq!(Format::GoCoverprofile.to_string(), "go-coverprofile");
    }

    #[test]
    fn parse_with_explicit_format_dispatches_each_parser() {
        let lcov = parse("SF:a.rs\nDA:1,1\nend_of_record\n", Some(Format::Lcov)).unwrap();
        assert_eq!(lcov.hits("a.rs", 1), Some(1));

        let json = parse(
            r#"{"data":[{"files":[{"filename":"a.rs","segments":[[1,1,3,true,true,false],[2,1,0,false,false,false]]}]}]}"#,
            Some(Format::LlvmCovJson),
        )
        .unwrap();
        assert_eq!(json.hits("a.rs", 1), Some(3));

        let xml = parse(
            r#"<coverage><packages><package><classes><class filename="a.rs"><lines><line number="1" hits="2"/></lines></class></classes></package></packages></coverage>"#,
            Some(Format::Cobertura),
        )
        .unwrap();
        assert_eq!(xml.hits("a.rs", 1), Some(2));

        let go = parse(
            "mode: count\nm/a.go:1.1,2.9 1 4\n",
            Some(Format::GoCoverprofile),
        )
        .unwrap();
        assert_eq!(go.hits("m/a.go", 2), Some(4));
    }

    #[test]
    fn resolve_prefers_an_explicit_format_over_detection() {
        assert_eq!(
            resolve("mode: set\n", Some(Format::Lcov)).unwrap(),
            Format::Lcov
        );
        assert_eq!(
            resolve("mode: set\n", None).unwrap(),
            Format::GoCoverprofile
        );
        assert!(resolve("", None).is_err());
    }

    #[test]
    fn parse_propagates_parser_errors() {
        // Detected as JSON but invalid → parse error with context.
        assert!(parse("{ not json", None).is_err());
    }

    /// Real `cargo llvm-cov` 0.8.7 output (rustc 1.98, LLVM 22.1.8, edition 2021)
    /// for this whole `src/lib.rs`, run under `cargo llvm-cov --no-report` and
    /// then `cargo llvm-cov report --lcov` / `--json`:
    ///
    /// ```text
    ///  1 pub fn generic<T: Default + PartialEq>(t: T) -> u8 {
    ///  2     if t == T::default() {
    ///  3         0
    ///  4     } else {
    ///  5         1
    ///  6     }
    ///  7 }
    ///  8
    ///  9 pub fn wrap(v: &[u8]) -> Vec<u8> {
    /// 10     v.iter().map(|b| b + 1).collect()
    /// 11 }
    /// 12
    /// 13 #[cfg(test)]
    /// 14 mod tests {
    /// 15     use super::*;
    /// 16
    /// 17     #[test]
    /// 18     fn t() {
    /// 19         assert_eq!(generic(0u8), 0);
    /// 20         assert_eq!(generic(1u16), 1);
    /// 21         assert_eq!(wrap(&[1]), vec![2]);
    /// 22     }
    /// 23 }
    /// ```
    ///
    /// Trimmed to the records and fields that matter here: the lcov's `FN*`/`BR*`
    /// records and the JSON's `functions`, `branches` and non-line summaries are
    /// elided, and the absolute path is replaced by `/repo`.
    ///
    /// Two things in it make llvm-cov's own summary disagree with the per-line
    /// records (#2131):
    ///
    /// - the closure `|b| b + 1` is a function record of its own that *also*
    ///   maps line 10, which `wrap` maps too, so the summary counts line 10
    ///   twice; and
    /// - `generic` has two instantiations (`u8` takes line 3, `u16` line 5). The
    ///   summary merges them by `max` (one line missed); the per-line view unions
    ///   them (every line covered).
    ///
    /// So the summary — and the lcov `LF`/`LH` records, which are written from
    /// it — say 14 lines, 13 covered, while the `DA` records and the JSON
    /// `segments` hold 13 lines, all covered.
    const LLVM_COV_GAP_LCOV: &str = "\
SF:/repo/src/lib.rs
DA:1,2
DA:2,2
DA:3,1
DA:5,1
DA:7,2
DA:9,1
DA:10,1
DA:11,1
DA:18,1
DA:19,1
DA:20,1
DA:21,1
DA:22,1
LF:14
LH:13
end_of_record";

    /// The JSON half of [`LLVM_COV_GAP_LCOV`].
    const LLVM_COV_GAP_JSON: &str = r#"{"data":[{"files":[{"filename":"/repo/src/lib.rs",
"segments":[
[1,1,2,true,true,false],[1,51,0,false,false,false],[2,8,2,true,true,false],
[2,25,0,false,false,false],[3,9,1,true,true,false],[3,10,0,false,false,false],
[5,9,1,true,true,false],[5,10,0,false,false,false],[7,1,2,true,true,false],
[7,2,0,false,false,false],[9,1,1,true,true,false],[9,33,0,false,false,false],
[10,5,1,true,true,false],[10,6,0,false,false,false],[10,7,1,true,true,false],
[10,11,0,false,false,false],[10,14,1,true,true,false],[10,17,0,false,false,false],
[10,22,1,true,true,false],[10,23,0,false,false,false],[10,29,1,true,true,false],
[10,36,0,false,false,false],[11,1,1,true,true,false],[11,2,0,false,false,false],
[18,5,1,true,true,false],[18,11,0,false,false,false],[19,9,1,true,true,false],
[19,19,0,false,false,false],[19,20,1,true,true,false],[19,27,0,false,false,false],
[20,9,1,true,true,false],[20,19,0,false,false,false],[20,20,1,true,true,false],
[20,27,0,false,false,false],[21,9,1,true,true,false],[21,19,0,false,false,false],
[21,20,1,true,true,false],[21,24,0,false,false,false],[21,25,1,true,true,false],
[21,29,0,false,false,false],[21,32,1,true,true,false],[21,36,0,false,false,false],
[22,5,1,true,true,false],[22,6,0,false,false,false]],
"summary":{"lines":{"count":14,"covered":13,"percent":92.85714285714286}}}],
"totals":{"lines":{"count":14,"covered":13,"percent":92.85714285714286}}}],
"type":"llvm.coverage.json.export","version":"3.1.0"}"#;

    /// `coverage diff`'s total is the per-line view — distinct lines — and not
    /// the figure `llvm-cov report` prints. The lcov carries that figure too, in
    /// `LF`/`LH`, so reading those instead of counting `DA` records would move
    /// every `--fail-under-lines` gate; this pins that it does not.
    #[test]
    fn lcov_total_counts_da_records_not_lf_and_lh() {
        assert!(LLVM_COV_GAP_LCOV.contains("\nLF:14\nLH:13\n"));

        let report = parse(LLVM_COV_GAP_LCOV, Some(Format::Lcov)).unwrap();

        assert_eq!(report.total_lines(), 13);
        assert_eq!(report.covered_lines(), 13);
    }

    /// The JSON export's `summary`/`totals` carry the summary's figure as well;
    /// the parser must keep rebuilding lines from `segments` and ignore them.
    #[test]
    fn llvm_json_total_is_rebuilt_from_segments_not_taken_from_summary() {
        assert!(LLVM_COV_GAP_JSON.contains(r#""lines":{"count":14,"covered":13"#));

        let report = parse(LLVM_COV_GAP_JSON, Some(Format::LlvmCovJson)).unwrap();

        assert_eq!(report.total_lines(), 13);
        assert_eq!(report.covered_lines(), 13);
    }

    /// An lcov written from the per-line view carries the same total on the way
    /// back in, and its `LF`/`LH` agree with its `DA` records — where the real
    /// output above disagrees with itself. This is what lets `coverage merge`
    /// promise the total `coverage diff` computes (#2118).
    #[test]
    fn written_lcov_keeps_the_per_line_total() {
        let report = parse(LLVM_COV_GAP_LCOV, Some(Format::Lcov)).unwrap();

        let written = lcov::write(&report).unwrap();
        let reread = parse(&written, Some(Format::Lcov)).unwrap();

        assert_eq!(reread, report);
        assert_eq!(reread.total_lines(), 13);
        assert_eq!(reread.covered_lines(), 13);
        assert!(written.contains("\nLF:13\nLH:13\n"), "{written}");
        assert!(!written.contains("LF:14"), "{written}");
    }

    /// Both real-world formats yield the same per-line view — the same
    /// executable lines with the same hit counts — which is what lets a sharded
    /// run mix them and a baseline in one format be compared with a head in the
    /// other.
    #[test]
    fn lcov_and_llvm_json_agree_line_for_line() {
        let lcov = parse(LLVM_COV_GAP_LCOV, Some(Format::Lcov)).unwrap();
        let json = parse(LLVM_COV_GAP_JSON, Some(Format::LlvmCovJson)).unwrap();

        assert_eq!(
            lcov.files["/repo/src/lib.rs"].lines,
            json.files["/repo/src/lib.rs"].lines
        );
    }
}
