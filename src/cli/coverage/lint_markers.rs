//! Standalone syntax check for source coverage markers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;
use git2::Repository;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::diff::load_coverage_config;
use crate::claude::context::resolve_context_dir_at;
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
            let selected = select_tracked(tracked_paths(&repo)?, include.as_ref());
            if include.is_some() && selected.is_empty() {
                // A typo in a glob must not look like a clean scan.
                eprintln!(
                    "warning: no tracked file matches the globs in {origin}; \
                     no coverage markers were checked"
                );
            }
            selected
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
/// Discovery is the one `coverage diff` uses without `--context-dir`:
/// `OMNI_DEV_CONFIG_DIR`, else a walk-up from the repository root, plus the
/// usual local-override and XDG/home fallbacks. A missing file is no
/// restriction; a malformed one is a hard error (a misspelled *key* is ignored,
/// as everywhere in this file, so the schema can grow).
fn load_config_include(repo_root: &Path) -> Result<Vec<String>> {
    let context_dir = resolve_context_dir_at(None, repo_root);
    Ok(load_coverage_config(&context_dir)?.lint_markers.include)
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
///
/// A path in a conflict has one entry per stage; it is listed once. A path that
/// is not valid UTF-8 cannot be matched against a glob or shown in a
/// diagnostic, so it is skipped like non-UTF-8 content is.
fn tracked_paths(repo: &Repository) -> Result<Vec<PathBuf>> {
    let index = repo.index().context("could not read Git index")?;
    let mut paths: Vec<PathBuf> = index
        .iter()
        .filter(|entry| REGULAR_FILE_MODES.contains(&entry.mode))
        .filter_map(|entry| std::str::from_utf8(&entry.path).ok().map(PathBuf::from))
        .collect();
    // The index is sorted by path, so a path's stages are adjacent.
    paths.dedup();
    Ok(paths)
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

    fn entry(path: &str, mode: u32, stage: u16, id: git2::Oid) -> git2::IndexEntry {
        git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            file_size: 0,
            id,
            flags: stage << 12,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        }
    }

    #[test]
    fn tracked_paths_keeps_regular_files_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let blob = repo.blob(b"x").unwrap();
        // A gitlink names a commit in another repository, so it has no object here.
        let commit = git2::Oid::from_str("0123456789012345678901234567890123456789").unwrap();
        let mut index = repo.index().unwrap();
        index.add(&entry("a.txt", 0o100_644, 0, blob)).unwrap();
        index.add(&entry("run.sh", 0o100_755, 0, blob)).unwrap();
        index.add(&entry("link", 0o120_000, 0, blob)).unwrap();
        index
            .add(&entry("vendor/lib", 0o160_000, 0, commit))
            .unwrap();
        // A conflicted path has one entry per stage.
        for stage in 1..=3 {
            index
                .add(&entry("conflict.md", 0o100_644, stage, blob))
                .unwrap();
        }
        assert_eq!(
            tracked_paths(&repo).unwrap(),
            paths(&["a.txt", "conflict.md", "run.sh"])
        );
    }
}
