//! AI scratch directory utilities.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::utils::env::{EnvSource, SystemEnv};

/// Resolves the scratch directory from an injected [`EnvSource`], deferring
/// git-root discovery to `git_root` (only invoked for the `git-root:` form).
///
/// This is the env-parsing core shared by [`get_ai_scratch_dir`] and
/// [`get_ai_scratch_dir_at`]; tests drive it with a pure `MapEnv` so the
/// `AI_SCRATCH` / `TMPDIR` branches are covered without mutating the
/// process-global environment (issue #1030).
fn resolve_scratch_dir(
    env: &impl EnvSource,
    git_root: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    // Check for AI_SCRATCH environment variable first
    if let Some(ai_scratch) = env.var("AI_SCRATCH") {
        if let Some(git_root_path) = ai_scratch.strip_prefix("git-root:") {
            // Find git root and append the path
            Ok(git_root()?.join(git_root_path))
        } else {
            // Use AI_SCRATCH directly
            Ok(PathBuf::from(ai_scratch))
        }
    } else {
        // Fall back to TMPDIR
        let tmpdir = env.var("TMPDIR").unwrap_or_else(|| "/tmp".to_string());
        Ok(PathBuf::from(tmpdir))
    }
}

/// Returns the AI scratch directory path based on environment variables and git root detection.
pub fn get_ai_scratch_dir() -> Result<PathBuf> {
    resolve_scratch_dir(&SystemEnv, find_git_root)
}

/// Returns the AI scratch directory, resolving the `git-root:` form against
/// `repo_root` instead of the process current working directory.
///
/// Behaves identically to [`get_ai_scratch_dir`] for the direct-path and
/// `TMPDIR` fallback cases; only the `git-root:` walk-up is anchored to the
/// injected `repo_root`.
pub fn get_ai_scratch_dir_at(repo_root: &Path) -> Result<PathBuf> {
    resolve_scratch_dir(&SystemEnv, || find_git_root_from_path(repo_root))
}

/// Finds the closest ancestor directory containing a .git directory.
fn find_git_root() -> Result<PathBuf> {
    let current_dir = env::current_dir().context("Failed to get current directory")?;
    find_git_root_from_path(&current_dir)
}

/// Finds the git root starting from a specific path.
fn find_git_root_from_path(start_path: &Path) -> Result<PathBuf> {
    let mut current = start_path;

    loop {
        let git_dir = current.join(".git");
        if git_dir.exists() {
            return Ok(current.to_path_buf());
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => {
                return Err(anyhow::anyhow!(
                    "No git repository found in current directory or any parent directory"
                ))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;
    use tempfile::TempDir;

    /// [`get_ai_scratch_dir_at`] over an injected [`EnvSource`], so the
    /// `AI_SCRATCH` / `TMPDIR` branches are tested without mutating the
    /// process environment. The `repo_root` is only consulted for the
    /// `git-root:` form.
    fn scratch_dir_with(env: &impl EnvSource, repo_root: &Path) -> Result<PathBuf> {
        resolve_scratch_dir(env, || super::find_git_root_from_path(repo_root))
    }

    #[test]
    fn get_ai_scratch_dir_with_direct_path() {
        let env = MapEnv::new().with("AI_SCRATCH", "/custom/scratch/path");

        let result = scratch_dir_with(&env, Path::new("/unused")).unwrap();
        assert_eq!(result, PathBuf::from("/custom/scratch/path"));
    }

    #[test]
    fn get_ai_scratch_dir_fallback_to_tmpdir() {
        // AI_SCRATCH absent → TMPDIR. A pure MapEnv means no process-global
        // TMPDIR mutation (which previously raced other tests' TempDir::new).
        let env = MapEnv::new().with("TMPDIR", "/custom/tmp");

        let result = scratch_dir_with(&env, Path::new("/unused")).unwrap();
        assert_eq!(result, PathBuf::from("/custom/tmp"));
    }

    #[test]
    fn get_ai_scratch_dir_at_resolves_git_root_from_injected_path() {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            TempDir::new_in("tmp").unwrap()
        };
        std::fs::create_dir(temp_dir.path().join(".git")).unwrap();
        let env = MapEnv::new().with("AI_SCRATCH", "git-root:scratch");

        // Resolves the `git-root:` form against the injected path, not the
        // process current working directory.
        let result = scratch_dir_with(&env, temp_dir.path()).unwrap();
        assert_eq!(result, temp_dir.path().join("scratch"));
    }

    #[test]
    fn public_wrappers_resolve_without_panicking() {
        // The thin `SystemEnv` wrappers read the real environment (no mutation,
        // no network, no side effects), so we exercise them for coverage and
        // assert only that resolution completes — the path depends on the
        // ambient AI_SCRATCH/TMPDIR, which we deliberately don't control here.
        let _ = get_ai_scratch_dir();
        let _ = get_ai_scratch_dir_at(Path::new("/tmp"));
    }

    #[test]
    fn find_git_root_from_path() {
        let temp_dir = {
            std::fs::create_dir_all("tmp").ok();
            TempDir::new_in("tmp").unwrap()
        };
        let git_dir = temp_dir.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();

        let sub_dir = temp_dir.path().join("subdir").join("deeper");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let result = super::find_git_root_from_path(&sub_dir).unwrap();
        assert_eq!(result, temp_dir.path());
    }
}
