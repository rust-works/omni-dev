//! Explicit report-path mappings for package and source-root-relative reports.

use std::collections::BTreeSet;

use anyhow::{ensure, Result};

use super::model::CoverageReport;

/// Replaces a report directory prefix with a repository-relative directory.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathMapping {
    /// Report prefix; an empty string matches every relative report path.
    pub from: String,
    /// Repository-relative destination; an empty string removes the prefix.
    pub to: String,
}

/// Validates mappings before any report is transformed.
pub(crate) fn validate(mappings: &[PathMapping]) -> Result<()> {
    let mut prefixes = BTreeSet::new();
    for mapping in mappings {
        let from = portable(&mapping.from);
        let to = portable(&mapping.to);
        ensure!(
            prefixes.insert(from),
            "duplicate coverage path-mappings source prefix: {}",
            mapping.from
        );
        ensure!(
            !to.starts_with('/') && !to.contains(':') && !to.split('/').any(|p| p == ".."),
            "coverage path-mappings destination must be repo-relative without '..': {}",
            mapping.to
        );
    }
    Ok(())
}

impl CoverageReport {
    /// Applies one longest directory-prefix mapping per path, merging aliases
    /// by maximum line hits. Unmatched paths are retained. Backslashes become
    /// slashes when mappings are enabled; no filesystem probing is performed.
    pub fn map_paths(&mut self, mappings: &[PathMapping]) -> Result<()> {
        self.map_paths_with_root_match(mappings).map(|_| ())
    }

    /// Returns whether an absolute report path matched an explicit mapping.
    /// Such a match counts as an accepted runner root for shard diagnostics.
    pub(crate) fn map_paths_with_root_match(&mut self, mappings: &[PathMapping]) -> Result<bool> {
        validate(mappings)?;
        if mappings.is_empty() {
            return Ok(false);
        }
        let mappings: Vec<_> = mappings
            .iter()
            .map(|m| (portable(&m.from), portable(&m.to)))
            .collect();
        let mut mapped_root = false;
        let files = std::mem::take(&mut self.files);
        for mut file in files.into_values() {
            let path = portable(&file.path);
            let best = mappings
                .iter()
                .filter_map(|(from, to)| remainder(&path, from).map(|rest| (from.len(), to, rest)))
                .max_by_key(|(len, _, _)| *len);
            mapped_root |= best.is_some() && (path.starts_with('/') || path.contains(':'));
            file.path = match best {
                Some((_, to, rest)) if to.is_empty() => rest.to_string(),
                Some((_, to, "")) => to.clone(),
                Some((_, to, rest)) => format!("{to}/{rest}"),
                None => path,
            };
            self.insert(file);
        }
        Ok(mapped_root)
    }
}

/// Slash-normalises a report prefix without interpreting parent components.
fn portable(path: &str) -> String {
    let path = path.replace('\\', "/");
    if path.starts_with('/') && path.trim_end_matches('/').is_empty() {
        return "/".to_string();
    }
    path.trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

/// Matches complete directory components, never `src` against `src2`.
fn remainder<'a>(path: &'a str, from: &str) -> Option<&'a str> {
    if from.is_empty() {
        return (!path.starts_with('/') && !path.contains(':')).then_some(path);
    }
    if from == "/" {
        return path.strip_prefix('/');
    }
    path.strip_prefix(from).and_then(|rest| {
        if rest.is_empty() {
            Some(rest)
        } else {
            rest.strip_prefix('/')
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::coverage::FileCoverage;

    fn mapping(from: &str, to: &str) -> PathMapping {
        PathMapping {
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn mappings_use_longest_boundary_match_without_chaining() {
        let mut report = CoverageReport::new();
        for path in ["src/a.js", "src/special/b.js", "src2/c.js", "/outside/a.js"] {
            let mut file = FileCoverage::new(path);
            file.record(1, 1);
            report.insert(file);
        }
        report
            .map_paths(&[
                mapping("", "fallback"),
                mapping("src", "packages/web/src"),
                mapping("src/special", "special"),
                mapping("special", "chained"),
            ])
            .unwrap();
        assert!(report.files.contains_key("packages/web/src/a.js"));
        assert!(report.files.contains_key("special/b.js"));
        assert!(report.files.contains_key("fallback/src2/c.js"));
        assert!(report.files.contains_key("/outside/a.js"));
    }

    #[test]
    fn mappings_normalise_windows_paths_and_merge_aliases() {
        let mut report = CoverageReport::new();
        for (path, hits) in [(r"C:\agent\repo\App.cs", 0), ("src/App.cs", 4)] {
            let mut file = FileCoverage::new(path);
            file.record(2, hits);
            report.insert(file);
        }
        report
            .map_paths(&[mapping(r"C:\agent\repo", "src")])
            .unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.hits("src/App.cs", 2), Some(4));
    }

    #[test]
    fn mappings_validate_before_mutating_report() {
        for to in [
            "/",
            "///",
            "../outside",
            "/absolute",
            "C:/absolute",
            "src/../outside",
        ] {
            assert!(validate(&[mapping("src", to)]).is_err());
        }
        assert!(validate(&[mapping("src/", "a"), mapping("./src", "b")]).is_err());
        let mut report = CoverageReport::new();
        report.insert(FileCoverage::new("src/a"));
        let before = report.clone();
        assert!(report.map_paths(&[mapping("src", "../bad")]).is_err());
        assert_eq!(report, before);
    }

    #[test]
    fn mappings_can_strip_import_prefix_and_prepend_source_root() {
        let mut report = CoverageReport::new();
        report.insert(FileCoverage::new("example.com/module/pkg/a.go"));
        report
            .map_paths(&[mapping("example.com/module", "services/api")])
            .unwrap();
        assert!(report.files.contains_key("services/api/pkg/a.go"));
        report.map_paths(&[mapping("services/api", "")]).unwrap();
        assert!(report.files.contains_key("pkg/a.go"));
        report.map_paths(&[mapping("", "src/main/java")]).unwrap();
        assert!(report.files.contains_key("src/main/java/pkg/a.go"));
    }
}
