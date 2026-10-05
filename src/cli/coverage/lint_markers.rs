//! Standalone syntax check for source coverage markers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;
use git2::Repository;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::diff::{CoverageConfig, COVERAGE_CONFIG_FILE};
use crate::claude::context::{load_config_content, resolve_context_dir_at};
use crate::coverage::markers;

/// Index modes of a regular file. A symlink (`0o120000`) or a gitlink
/// (`0o160000`, a submodule — a directory in the worktree) is not source text.
const REGULAR_FILE_MODES: [u32; 2] = [0o100_644, 0o100_755];

/// Checks source coverage markers without generating a coverage report.
#[derive(Parser)]
pub struct LintMarkersCommand {
    /// Files to scan (default: every tracked text file in the repository).
    ///
    /// A file that is not valid UTF-8 is skipped.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Scan only tracked files matching this glob (repeatable).
    ///
    /// Matched against the repo-relative, `/`-separated path: `*` stays within
    /// one path component, `**` crosses directories (`src/**/*.py`). Replaces
    /// `lint-markers.include` in `.omni-dev/coverage.yaml`. Ignored when PATHs
    /// are given, since those are an explicit selection.
    #[arg(long = "include", value_name = "GLOB")]
    pub include: Vec<String>,
}

impl LintMarkersCommand {
    /// Checks each selected file's working-tree contents.
    pub fn execute(self, repo_path: Option<&Path>) -> Result<()> {
        let repo = Repository::discover(repo_path.unwrap_or_else(|| Path::new(".")))
            .context("could not find a Git repository for coverage marker lint")?;
        let root = repo
            .workdir()
            .context("coverage marker lint requires a Git working tree")?;
        let tracked = self.paths.is_empty();
        let paths = if tracked {
            let (patterns, origin) = if self.include.is_empty() {
                (
                    load_config_include(root)?,
                    "lint-markers.include in coverage.yaml",
                )
            } else {
                (self.include, "--include")
            };
            let include = compile_include(&patterns, origin)?;
            select_tracked(tracked_paths(&repo)?, include.as_ref())
        } else {
            self.paths
        };

        let mut failed = false;
        for path in paths {
            let file = root.join(&path);
            // A tracked file deleted from the worktree has no current source to lint.
            if tracked
                && matches!(
                    std::fs::symlink_metadata(&file),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                )
            {
                continue;
            }
            let bytes = std::fs::read(&file)
                .with_context(|| format!("could not read {}", file.display()))?;
            // Binary (non-UTF-8) content is never flagged: it can only fail to
            // find a marker, which is what `coverage diff` assumes of it too.
            let Ok(source) = String::from_utf8(bytes) else {
                continue;
            };
            let display = path.display().to_string();
            if let Err(error) = markers::scan(&display, &source) {
                eprintln!("{error:#}");
                failed = true;
            }
        }
        if failed {
            bail!("coverage marker lint failed");
        }
        Ok(())
    }
}

/// Loads `lint-markers.include` from the discovered `.omni-dev/coverage.yaml`.
///
/// Discovery is the one `coverage diff` uses: `OMNI_DEV_CONFIG_DIR`, else a
/// walk-up from the repository root. A missing file is no restriction; a
/// present-but-malformed one is a hard error, so a typo cannot quietly widen the
/// scan back to every file.
fn load_config_include(repo_root: &Path) -> Result<Vec<String>> {
    let context_dir = resolve_context_dir_at(None, repo_root);
    let Some(content) = load_config_content(&context_dir, COVERAGE_CONFIG_FILE)? else {
        return Ok(Vec::new());
    };
    let config: CoverageConfig = serde_yaml::from_str(&content).with_context(|| {
        format!(
            "could not parse coverage config {}/{COVERAGE_CONFIG_FILE}",
            context_dir.display()
        )
    })?;
    Ok(config.lint_markers.include)
}

/// Compiles include globs into a set, or `None` when there are none (no
/// restriction). `*` does not cross `/`, so `*.py` means a top-level file and
/// `**/*.py` means any.
fn compile_include(patterns: &[String], origin: &str) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid glob `{pattern}` in {origin}"))?;
        builder.add(glob);
    }
    Ok(Some(builder.build().with_context(|| {
        format!("could not compile the globs in {origin}")
    })?))
}

/// Keeps the paths the include set admits (all of them when there is none).
fn select_tracked(paths: Vec<PathBuf>, include: Option<&GlobSet>) -> Vec<PathBuf> {
    match include {
        Some(set) => paths.into_iter().filter(|p| set.is_match(p)).collect(),
        None => paths,
    }
}

/// Lists regular files in the index, including staged additions.
fn tracked_paths(repo: &Repository) -> Result<Vec<PathBuf>> {
    let index = repo.index().context("could not read Git index")?;
    index
        .iter()
        .filter(|entry| REGULAR_FILE_MODES.contains(&entry.mode))
        .map(|entry| {
            let path = std::str::from_utf8(&entry.path).context("tracked path is not UTF-8")?;
            Ok(PathBuf::from(path))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn globs(patterns: &[&str]) -> GlobSet {
        let patterns: Vec<String> = patterns.iter().map(ToString::to_string).collect();
        compile_include(&patterns, "test").unwrap().unwrap()
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn no_patterns_means_no_restriction() {
        assert!(compile_include(&[], "test").unwrap().is_none());
        let all = paths(&["a.rs", "docs/b.md"]);
        assert_eq!(select_tracked(all.clone(), None), all);
    }

    #[test]
    fn star_stays_in_one_component_and_double_star_crosses() {
        let set = globs(&["src/**/*.py", "*.toml"]);
        let kept = select_tracked(
            paths(&[
                "src/a.py",
                "src/x/y/b.py",
                "src/c.rs",
                "Cargo.toml",
                "sub/Cargo.toml",
            ]),
            Some(&set),
        );
        assert_eq!(kept, paths(&["src/a.py", "src/x/y/b.py", "Cargo.toml"]));
    }

    #[test]
    fn patterns_union() {
        let set = globs(&["**/*.rs", "**/*.py"]);
        let kept = select_tracked(paths(&["a.rs", "d/b.py", "c.md"]), Some(&set));
        assert_eq!(kept, paths(&["a.rs", "d/b.py"]));
    }

    #[test]
    fn invalid_glob_names_the_pattern_and_its_source() {
        let error = compile_include(&["src/[".to_string()], "--include").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("src/["), "{message}");
        assert!(message.contains("--include"), "{message}");
    }

    #[test]
    fn only_regular_file_modes_are_scanned() {
        assert!(REGULAR_FILE_MODES.contains(&0o100_644));
        assert!(REGULAR_FILE_MODES.contains(&0o100_755));
        assert!(!REGULAR_FILE_MODES.contains(&0o120_000));
        assert!(!REGULAR_FILE_MODES.contains(&0o160_000));
    }
}
