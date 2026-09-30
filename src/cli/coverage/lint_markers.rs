//! Standalone syntax check for source coverage markers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;
use git2::Repository;

use crate::coverage::markers;

/// Checks source coverage markers without generating a coverage report.
#[derive(Parser)]
pub struct LintMarkersCommand {
    /// Files to scan (default: every tracked Rust file in the repository).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
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
            tracked_rust_paths(&repo)?
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
            let source = std::fs::read_to_string(&file)
                .with_context(|| format!("could not read {}", file.display()))?;
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

/// Lists Rust files in the index, including staged additions.
fn tracked_rust_paths(repo: &Repository) -> Result<Vec<PathBuf>> {
    let index = repo.index().context("could not read Git index")?;
    index
        .iter()
        .filter(|entry| entry.path.ends_with(b".rs"))
        .map(|entry| {
            let path =
                std::str::from_utf8(&entry.path).context("tracked Rust path is not UTF-8")?;
            Ok(PathBuf::from(path))
        })
        .collect()
}
