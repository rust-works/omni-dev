//! Cobertura XML coverage parser (line coverage only).
//!
//! Cobertura records coverage under `<class filename="…">` elements, each
//! containing `<lines><line number="N" hits="H"/></lines>`. A single source
//! file may be split across several `<class>` elements (one per class/closure);
//! [`CoverageReport::insert`][super::model::CoverageReport::insert] merges them.
//! Branch (`condition-coverage`) data is ignored — v1 is line coverage only.

use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::model::{CoverageReport, FileCoverage};

/// Parses cobertura XML text into a [`CoverageReport`].
pub fn parse(content: &str) -> Result<CoverageReport> {
    let mut reader = Reader::from_str(content);
    let mut report = CoverageReport::new();
    let mut current: Option<FileCoverage> = None;

    loop {
        match reader.read_event().context("malformed cobertura XML")? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                handle_start(&e, &mut current, &mut report)?;
            }
            Event::End(e) => {
                if e.name().as_ref() == b"class" {
                    if let Some(file) = current.take() {
                        report.insert(file);
                    }
                }
            }
            _ => {}
        }
    }

    // Tolerate a final class with no explicit close.
    if let Some(file) = current.take() {
        report.insert(file);
    }

    Ok(report)
}

/// Handles a `<class>` (opens a file) or `<line>` (records a hit) start tag.
fn handle_start(
    e: &BytesStart,
    current: &mut Option<FileCoverage>,
    report: &mut CoverageReport,
) -> Result<()> {
    match e.name().as_ref() {
        b"class" => {
            // A new <class> closes any previous one missing an explicit </class>.
            if let Some(file) = current.take() {
                report.insert(file);
            }
            if let Some(filename) = attr(e, b"filename")? {
                *current = Some(FileCoverage::new(filename));
            }
        }
        b"line" => {
            if let Some(file) = current.as_mut() {
                let number = attr(e, b"number")?
                    .and_then(|s| s.parse::<u32>().ok())
                    .context("cobertura <line> missing/invalid number attribute")?;
                let hits = attr(e, b"hits")?
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                file.record(number, hits);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Reads attribute `key` off `e`, unescaping its value.
fn attr(e: &BytesStart, key: &[u8]) -> Result<Option<String>> {
    for attr in e.attributes() {
        let attr = attr.context("malformed cobertura attribute")?;
        if attr.key.as_ref() == key {
            let value = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .context("invalid cobertura attribute value")?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_classes_and_lines() {
        let xml = r#"<?xml version="1.0" ?>
<coverage>
  <packages>
    <package name="omni-dev">
      <classes>
        <class name="a" filename="src/a.rs">
          <lines>
            <line number="1" hits="5"/>
            <line number="2" hits="0"/>
          </lines>
        </class>
        <class name="b" filename="src/b.rs">
          <lines>
            <line number="1" hits="1"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;
        let report = parse(xml).unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.hits("src/a.rs", 1), Some(5));
        assert_eq!(report.hits("src/a.rs", 2), Some(0));
        assert_eq!(report.hits("src/b.rs", 1), Some(1));
    }

    #[test]
    fn merges_split_file_classes() {
        let xml = r#"<coverage><packages><package><classes>
            <class filename="src/a.rs"><lines><line number="1" hits="0"/></lines></class>
            <class filename="src/a.rs"><lines><line number="2" hits="3"/></lines></class>
        </classes></package></packages></coverage>"#;
        let report = parse(xml).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.hits("src/a.rs", 1), Some(0));
        assert_eq!(report.hits("src/a.rs", 2), Some(3));
    }

    #[test]
    fn ignores_branch_conditions() {
        let xml = r#"<coverage><packages><package><classes>
            <class filename="src/a.rs"><lines>
              <line number="1" hits="2" branch="true" condition-coverage="50% (1/2)">
                <conditions><condition number="0" type="jump" coverage="50%"/></conditions>
              </line>
            </lines></class>
        </classes></package></packages></coverage>"#;
        let report = parse(xml).unwrap();
        let f = &report.files["src/a.rs"];
        assert_eq!(f.lines.len(), 1);
        assert_eq!(f.lines.get(&1), Some(&2));
    }
}
