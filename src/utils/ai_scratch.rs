//! AI scratch directory utilities

use anyhow::{Context, Result};
use std::env;
use std::path::{Path, PathBuf};

/// Get the AI scratch directory path based on environment variables and git root detection
pub fn get_ai_scratch_dir() -> Result<PathBuf> {
    // Check for AI_SCRATCH environment variable first
    if let Ok(ai_scratch) = env::var("AI_SCRATCH") {
        if let Some(git_root_path) = ai_scratch.strip_prefix("git-root:") {
            // Find git root and append the path
            let git_root = find_git_root()?;
            Ok(git_root.join(git_root_path))
        } else {
            // Use AI_SCRATCH directly
            Ok(PathBuf::from(ai_scratch))
        }
    } else {
        // Fall back to TMPDIR
        let tmpdir = env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        Ok(PathBuf::from(tmpdir))
    }
}

/// Find the closest ancestor directory containing a .git directory
fn find_git_root() -> Result<PathBuf> {
    let current_dir = env::current_dir().context("Failed to get current directory")?;
    find_git_root_from_path(&current_dir)
}

/// Find git root starting from a specific path
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
mod tests {
    use super::*;
    use std::env;
    use tempfile::TempDir;

    use std::sync::Mutex;
    use std::sync::OnceLock;

    /// Global lock to ensure environment variable tests don't interfere with each other
    static ENV_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    /// Helper to manage environment variables in tests to avoid interference
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        vars: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = ENV_TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
            Self {
                _lock: lock,
                vars: Vec::new(),
            }
        }

        fn set(&mut self, key: &str, value: &str) {
            let original = env::var(key).ok();
            self.vars.push((key.to_string(), original));
            env::set_var(key, value);
        }

        fn remove(&mut self, key: &str) {
            let original = env::var(key).ok();
            self.vars.push((key.to_string(), original));
            env::remove_var(key);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore in reverse order
            for (key, original_value) in self.vars.drain(..).rev() {
                match original_value {
                    Some(value) => env::set_var(&key, value),
                    None => env::remove_var(&key),
                }
            }
        }
    }

    #[test]
    fn test_get_ai_scratch_dir_with_direct_path() {
        let mut guard = EnvGuard::new();
        guard.set("AI_SCRATCH", "/custom/scratch/path");

        let result = get_ai_scratch_dir().unwrap();
        assert_eq!(result, PathBuf::from("/custom/scratch/path"));
    }

    #[test]
    fn test_get_ai_scratch_dir_fallback_to_tmpdir() {
        let mut guard = EnvGuard::new();
        guard.remove("AI_SCRATCH");
        guard.set("TMPDIR", "/custom/tmp");

        let result = get_ai_scratch_dir().unwrap();
        assert_eq!(result, PathBuf::from("/custom/tmp"));
    }

    #[test]
    fn test_find_git_root_from_path() {
        let _guard = EnvGuard::new(); // Ensure clean environment

        let temp_dir = TempDir::new().unwrap();
        let git_dir = temp_dir.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();

        let sub_dir = temp_dir.path().join("subdir").join("deeper");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let result = find_git_root_from_path(&sub_dir).unwrap();
        assert_eq!(result, temp_dir.path());
    }
}
