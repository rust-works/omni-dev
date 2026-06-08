//! Coverage report format detection and parse dispatch.

use std::fmt;

use anyhow::{Context, Result};

use super::model::CoverageReport;
use super::{cobertura, lcov, llvm_json};

/// A supported per-line coverage report format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// lcov trace file (`DA`/`SF`/`end_of_record`).
    Lcov,
    /// llvm-cov JSON export (`cargo llvm-cov report --json`).
    LlvmCovJson,
    /// Cobertura XML.
    Cobertura,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Lcov => "lcov",
            Self::LlvmCovJson => "llvm-cov-json",
            Self::Cobertura => "cobertura",
        };
        f.write_str(name)
    }
}

impl Format {
    /// Detects the format from report `content`.
    ///
    /// Detection is by leading non-whitespace character/token: XML opens with
    /// `<`, JSON with `{`, and lcov with a record keyword (`TN:`/`SF:`).
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
            _ => anyhow::bail!(
                "could not auto-detect coverage report format; pass an explicit --report-format \
                 (lcov, llvm-cov-json, or cobertura)"
            ),
        }
    }

    /// Parses `content` according to this format.
    pub fn parse(self, content: &str) -> Result<CoverageReport> {
        match self {
            Self::Lcov => lcov::parse(content),
            Self::LlvmCovJson => llvm_json::parse(content),
            Self::Cobertura => cobertura::parse(content),
        }
    }
}

/// Parses `content` using `format`, auto-detecting when `format` is `None`.
pub fn parse(content: &str, format: Option<Format>) -> Result<CoverageReport> {
    let format = match format {
        Some(f) => f,
        None => Format::detect(content).context("coverage report format auto-detection failed")?,
    };
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
    }

    #[test]
    fn parse_propagates_parser_errors() {
        // Detected as JSON but invalid → parse error with context.
        assert!(parse("{ not json", None).is_err());
    }
}
