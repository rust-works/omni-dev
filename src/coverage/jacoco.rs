//! JaCoCo XML line coverage parser.
//!
//! Paths are relative to the source root: package name plus sourcefile name.
//! Covered instruction counts are converted to boolean hits, not execution
//! counts. Branch counters and class/method summaries are ignored. Aggregate
//! groups and repeated source paths merge by covered-line union.

use anyhow::{ensure, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::model::{CoverageReport, FileCoverage};

/// Parses JaCoCo XML into a report with source-root-relative paths.
///
/// A line is executable when it has missed or covered instructions, and is
/// covered when `ci > 0`. No external DTD or entity is fetched.
pub fn parse(content: &str) -> Result<CoverageReport> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().expand_empty_elements = true;
    let mut state = Parser::default();
    loop {
        match reader.read_event().context("Failed to read JaCoCo XML")? {
            Event::Start(e) => {
                state.start(&e)?;
                state.elements.push(e.name().as_ref().to_vec());
            }
            Event::End(e) => {
                ensure!(
                    state.elements.pop().as_deref() == Some(e.name().as_ref()),
                    "Mismatched JaCoCo XML closing tag"
                );
                match e.name().as_ref() {
                    b"sourcefile" => {
                        if let Some(file) = state.file.take() {
                            state.report.insert(file);
                        }
                    }
                    b"package" => state.package = None,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(state.root_seen, "JaCoCo XML is missing its <report> root");
    ensure!(state.elements.is_empty(), "Unclosed JaCoCo XML element");
    Ok(state.report)
}

#[derive(Default)]
struct Parser {
    report: CoverageReport,
    elements: Vec<Vec<u8>>,
    root_seen: bool,
    package: Option<String>,
    file: Option<FileCoverage>,
}

impl Parser {
    fn start(&mut self, e: &BytesStart<'_>) -> Result<()> {
        let parent = self.elements.last().map(Vec::as_slice);
        if parent.is_none() {
            ensure!(
                !self.root_seen && e.name().as_ref() == b"report",
                "Expected a single JaCoCo <report> root"
            );
            self.root_seen = true;
        }
        match e.name().as_ref() {
            b"package" => {
                ensure!(
                    matches!(parent, Some(b"report" | b"group")),
                    "JaCoCo <package> must be inside a report or group"
                );
                self.package = Some(attribute(e, b"name")?);
            }
            b"sourcefile" => {
                ensure!(
                    parent == Some(b"package"),
                    "JaCoCo <sourcefile> must be inside a package"
                );
                let name = attribute(e, b"name")?;
                ensure!(!name.is_empty(), "JaCoCo sourcefile name is empty");
                let package = self.package.as_deref().context("Missing JaCoCo package")?;
                let path = if package.is_empty() {
                    name
                } else {
                    format!("{package}/{name}")
                };
                self.file = Some(FileCoverage::new(path));
            }
            b"line" if parent == Some(b"sourcefile") => {
                let number = attribute(e, b"nr")?
                    .parse::<u32>()
                    .context("Invalid JaCoCo line number (nr)")?;
                ensure!(number > 0, "JaCoCo line number (nr) must be positive");
                let missed = attribute(e, b"mi")?
                    .parse::<u64>()
                    .context("Invalid JaCoCo missed instruction count (mi)")?;
                let covered = attribute(e, b"ci")?
                    .parse::<u64>()
                    .context("Invalid JaCoCo covered instruction count (ci)")?;
                if missed > 0 || covered > 0 {
                    if let Some(file) = self.file.as_mut() {
                        file.record(number, u64::from(covered > 0));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn attribute(e: &BytesStart<'_>, key: &[u8]) -> Result<String> {
    for attr in e.attributes() {
        let attr = attr.context("Malformed JaCoCo XML attribute")?;
        if attr.key.as_ref() == key {
            return Ok(attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .context("Invalid JaCoCo XML attribute value")?
                .into_owned());
        }
    }
    anyhow::bail!("Missing JaCoCo {} attribute", String::from_utf8_lossy(key))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Test fixtures and assertions may panic on failure.
mod tests {
    use super::*;

    #[test]
    fn multiple_packages_partial_lines_and_default_package() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE report PUBLIC "-//JACOCO//DTD Report 1.1//EN" "report.dtd">
<report name="demo"><package name="com/example">
<class name="com/example/App"><method name="main"><counter type="LINE" missed="1" covered="1"/></method></class>
<sourcefile name="App.java">
<line nr="1" mi="0" ci="7" mb="0" cb="0"/>
<line nr="2" mi="3" ci="0" mb="2" cb="0"/>
<line nr="3" mi="2" ci="1" mb="1" cb="1"></line>
<line nr="4" mi="0" ci="0" mb="0" cb="0"/>
</sourcefile></package><package name="other"><sourcefile name="App.kt">
<line nr="9" mi="1" ci="0"/></sourcefile></package>
<package name=""><sourcefile name="Main.scala"><line nr="2" mi="0" ci="2"/></sourcefile></package></report>"#;
        let r = parse(xml).unwrap();
        assert_eq!(r.files.len(), 3);
        assert_eq!(r.hits("com/example/App.java", 1), Some(1));
        assert_eq!(r.hits("com/example/App.java", 2), Some(0));
        assert_eq!(r.hits("com/example/App.java", 3), Some(1));
        assert_eq!(r.hits("com/example/App.java", 4), None);
        assert_eq!(r.hits("other/App.kt", 9), Some(0));
        assert_eq!(r.hits("Main.scala", 2), Some(1));
        assert_eq!(r.total_lines(), 5);
        assert_eq!(r.percent(), Some(60.0));
    }

    #[test]
    fn groups_and_separate_modules_merge_by_union() {
        let source = |hits| {
            format!(
                r#"<package name="p"><sourcefile name="A.java"><line nr="1" mi="1" ci="{hits}"/></sourcefile></package>"#
            )
        };
        let mut r = parse(&format!(r#"<report><group name="a">{}</group><group name="b"><group name="nested">{}</group></group></report>"#, source(0), source(3))).unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.hits("p/A.java", 1), Some(1));
        r.merge(parse(r#"<report><package name="p"><sourcefile name="A.java"><line nr="1" mi="1" ci="0"/><line nr="2" mi="1" ci="0"/></sourcefile></package></report>"#).unwrap());
        assert_eq!(r.hits("p/A.java", 1), Some(1));
        assert_eq!(r.hits("p/A.java", 2), Some(0));
    }

    #[test]
    fn empty_reports_files_and_escaped_names() {
        assert!(parse("<report/>").unwrap().files.is_empty());
        let r = parse(r#"<report><package name="a&amp;b"><sourcefile name="A&#46;java"/><sourcefile name="B.java"><line nr="1" mi="0" ci="1"/></sourcefile></package></report>"#).unwrap();
        assert!(r.files.contains_key("a&b/A.java"));
        assert_eq!(r.hits("a&b/B.java", 1), Some(1));
        assert_eq!(r.hits("a&b/A.java", 1), None);
    }

    #[test]
    fn invalid_line_attributes_fail_instead_of_changing_coverage() {
        for attrs in [
            r#"mi="1" ci="0""#,
            r#"nr="0" mi="1" ci="0""#,
            r#"nr="-1" mi="1" ci="0""#,
            r#"nr="x" mi="1" ci="0""#,
            r#"nr="4294967296" mi="1" ci="0""#,
            r#"nr="1" ci="0""#,
            r#"nr="1" mi="1""#,
            r#"nr="1" mi="-1" ci="0""#,
            r#"nr="1" mi="1" ci="no""#,
            r#"nr="1" mi="1" ci="18446744073709551616""#,
        ] {
            let xml = format!(
                r#"<report><package name="p"><sourcefile name="A.java"><line {attrs}/></sourcefile></package></report>"#
            );
            assert!(parse(&xml).is_err(), "{attrs}");
        }
    }

    #[test]
    fn invalid_attributes_and_unclosed_sourcefiles_fail() {
        for xml in [
            r#"<report><package name="p"><sourcefile name=""/></package></report>"#,
            r#"<report><package name="&unknown;"/></report>"#,
            r#"<report><package name "p"/></report>"#,
            r#"<report><package name="p"><sourcefile name="A.java"><line nr="1" mi="0" ci="1"/>"#,
        ] {
            assert!(parse(xml).is_err(), "{xml}");
        }
    }

    #[test]
    fn malformed_xml_and_missing_paths_fail() {
        for xml in [
            "",
            "<coverage/>",
            "<report",
            "<report>",
            "<report></package>",
            "<report/><report/>",
            "<report><package/></report>",
            r#"<report><package name="p"><sourcefile/></package></report>"#,
            r#"<report><sourcefile name="A.java"/></report>"#,
        ] {
            assert!(parse(xml).is_err(), "{xml}");
        }
    }
}
