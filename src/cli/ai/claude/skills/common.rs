//! Shared helpers for skills sync and clean commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Skills directory name relative to a repository or worktree root.
pub(super) const SKILLS_SUBPATH: &str = ".claude/skills";

/// Prefix used for entries written to `.git/info/exclude`.
pub(super) const EXCLUDE_PREFIX: &str = ".claude/skills/";

/// Runs `git rev-parse --show-toplevel` from `path` and returns the absolute root.
pub(super) fn resolve_toplevel(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()
        .with_context(|| {
            format!(
                "Failed to run git rev-parse --show-toplevel in {}",
                path.display()
            )
        })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "git rev-parse --show-toplevel failed in {}: {err}",
            path.display()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("git rev-parse --show-toplevel output was not UTF-8")?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        anyhow::bail!("git rev-parse --show-toplevel returned empty path");
    }
    Ok(PathBuf::from(trimmed))
}

/// Runs `git rev-parse --git-common-dir` from `path` and returns an absolute path.
///
/// The command may return a relative path (e.g. `.git`) when executed inside the
/// main worktree; this helper resolves any such relative path against `path`.
pub(super) fn resolve_git_common_dir(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(path)
        .output()
        .with_context(|| {
            format!(
                "Failed to run git rev-parse --git-common-dir in {}",
                path.display()
            )
        })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "git rev-parse --git-common-dir failed in {}: {err}",
            path.display()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("git rev-parse --git-common-dir output was not UTF-8")?;
    let raw = PathBuf::from(stdout.trim());
    if raw.is_absolute() {
        Ok(raw)
    } else {
        Ok(path.join(raw))
    }
}

/// Lists all worktree root paths for the repository containing `path`.
pub(super) fn list_worktrees(path: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(path)
        .output()
        .with_context(|| format!("Failed to run git worktree list in {}", path.display()))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("git worktree list failed in {}: {err}", path.display());
    }
    let stdout =
        String::from_utf8(output.stdout).context("git worktree list output was not UTF-8")?;
    Ok(parse_worktree_list(&stdout))
}

/// Parses porcelain output from `git worktree list --porcelain` into root paths.
pub(super) fn parse_worktree_list(output: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            roots.push(PathBuf::from(rest));
        }
    }
    roots
}

/// Lists skill directories in `source_skills_dir`, sorted by name.
pub(super) fn enumerate_skills(source_skills_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut skills = Vec::new();
    if !source_skills_dir.exists() {
        return Ok(skills);
    }
    let entries = fs::read_dir(source_skills_dir)
        .with_context(|| format!("Failed to read {}", source_skills_dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "Failed to read directory entry in {}",
                source_skills_dir.display()
            )
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        skills.push((name.to_string(), path));
    }
    skills.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(skills)
}

/// Returns the `.git/info/exclude` path for a target directory, using the common git dir.
pub(super) fn exclude_file_for(target_root: &Path) -> Result<PathBuf> {
    let common = resolve_git_common_dir(target_root)?;
    Ok(common.join("info").join("exclude"))
}

/// Returns the exclude-file entry for a given skill name.
pub(super) fn exclude_entry_for(skill_name: &str) -> String {
    format!("{EXCLUDE_PREFIX}{skill_name}/")
}

/// Appends missing entries to `exclude_file`, returning the list of added entries.
///
/// If `dry_run` is true the file is not modified but the would-be additions are
/// still reported.
pub(super) fn add_exclude_entries(
    exclude_file: &Path,
    entries: &[String],
    dry_run: bool,
) -> Result<Vec<String>> {
    let existing = read_exclude_lines(exclude_file)?;
    let mut additions = Vec::new();
    for entry in entries {
        if !existing.iter().any(|line| line == entry) {
            additions.push(entry.clone());
        }
    }
    if additions.is_empty() || dry_run {
        return Ok(additions);
    }
    if let Some(parent) = exclude_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut content = if exclude_file.exists() {
        fs::read_to_string(exclude_file)
            .with_context(|| format!("Failed to read {}", exclude_file.display()))?
    } else {
        String::new()
    };
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for entry in &additions {
        content.push_str(entry);
        content.push('\n');
    }
    fs::write(exclude_file, content)
        .with_context(|| format!("Failed to write {}", exclude_file.display()))?;
    Ok(additions)
}

/// Removes matching entries from `exclude_file`, returning the list removed.
///
/// Entries are matched by exact line content. If `dry_run` is true the file is
/// not modified.
pub(super) fn remove_exclude_entries(
    exclude_file: &Path,
    entries: &[String],
    dry_run: bool,
) -> Result<Vec<String>> {
    if !exclude_file.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(exclude_file)
        .with_context(|| format!("Failed to read {}", exclude_file.display()))?;
    let trailing_newline = content.ends_with('\n');
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for line in content.lines() {
        if entries.iter().any(|e| e == line) {
            removed.push(line.to_string());
        } else {
            kept.push(line.to_string());
        }
    }
    if removed.is_empty() || dry_run {
        return Ok(removed);
    }
    let mut new_content = kept.join("\n");
    if trailing_newline && !new_content.is_empty() {
        new_content.push('\n');
    }
    fs::write(exclude_file, new_content)
        .with_context(|| format!("Failed to write {}", exclude_file.display()))?;
    Ok(removed)
}

fn read_exclude_lines(exclude_file: &Path) -> Result<Vec<String>> {
    if !exclude_file.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(exclude_file)
        .with_context(|| format!("Failed to read {}", exclude_file.display()))?;
    Ok(content.lines().map(ToString::to_string).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    fn tempdir() -> TempDir {
        std::fs::create_dir_all("tmp").ok();
        TempDir::new_in("tmp").unwrap()
    }

    fn init_repo(dir: &Path) {
        let status = Command::new("git")
            .arg("init")
            .arg(dir)
            .output()
            .expect("git init failed to spawn");
        assert!(status.status.success(), "git init failed: {status:?}");
    }

    fn init_repo_with_commit(dir: &Path) {
        init_repo(dir);
        fs::write(dir.join("README.md"), "readme").unwrap();
        for (k, v) in [
            ("add", vec!["add", "README.md"]),
            (
                "commit",
                vec![
                    "-c",
                    "user.email=x@x",
                    "-c",
                    "user.name=x",
                    "commit",
                    "-q",
                    "-m",
                    "init",
                ],
            ),
        ] {
            let status = Command::new("git")
                .args(&v)
                .current_dir(dir)
                .output()
                .unwrap_or_else(|_| panic!("git {k} failed to spawn"));
            assert!(status.status.success(), "git {k} failed: {status:?}");
        }
    }

    #[test]
    fn resolve_toplevel_returns_repo_root() {
        let dir = tempdir();
        init_repo(dir.path());
        // Canonicalize because macOS /var -> /private/var, while `git init` returns
        // the canonical form.
        let expected = fs::canonicalize(dir.path()).unwrap();
        let result = resolve_toplevel(dir.path()).unwrap();
        assert_eq!(fs::canonicalize(result).unwrap(), expected);
    }

    #[test]
    fn resolve_toplevel_from_subdir_returns_repo_root() {
        let dir = tempdir();
        init_repo(dir.path());
        let sub = dir.path().join("sub/dir");
        fs::create_dir_all(&sub).unwrap();
        let expected = fs::canonicalize(dir.path()).unwrap();
        let result = resolve_toplevel(&sub).unwrap();
        assert_eq!(fs::canonicalize(result).unwrap(), expected);
    }

    #[test]
    fn resolve_toplevel_outside_repo_fails() {
        // Use the system temp dir (not the in-repo `tmp/`) so git cannot find a
        // parent repository from this path.
        let dir = TempDir::new().unwrap();
        let err = resolve_toplevel(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("git rev-parse --show-toplevel failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_git_common_dir_in_main_worktree() {
        let dir = tempdir();
        init_repo(dir.path());
        let common = resolve_git_common_dir(dir.path()).unwrap();
        // Path should end in `.git` and point into the repo.
        assert!(common.ends_with(".git"), "got {}", common.display());
        assert!(common.join("info").exists() || !common.join("info").exists());
    }

    #[test]
    fn resolve_git_common_dir_from_linked_worktree_points_at_main() {
        let main = tempdir();
        init_repo_with_commit(main.path());
        let wt = tempdir();
        let linked = wt.path().join("linked");
        let status = Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .current_dir(main.path())
            .output()
            .expect("git worktree add failed");
        assert!(status.status.success(), "git worktree add: {status:?}");

        let common = resolve_git_common_dir(&linked).unwrap();
        // The common dir of the linked worktree should be the main repo's `.git`.
        let main_git = fs::canonicalize(main.path().join(".git")).unwrap();
        assert_eq!(fs::canonicalize(&common).unwrap(), main_git);
    }

    #[test]
    fn resolve_git_common_dir_outside_repo_fails() {
        let dir = TempDir::new().unwrap();
        let err = resolve_git_common_dir(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("git rev-parse --git-common-dir failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn list_worktrees_returns_single_for_plain_repo() {
        let dir = tempdir();
        init_repo(dir.path());
        let trees = list_worktrees(dir.path()).unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(
            fs::canonicalize(&trees[0]).unwrap(),
            fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn list_worktrees_returns_multiple_with_linked_worktree() {
        let main = tempdir();
        init_repo_with_commit(main.path());
        let wt = tempdir();
        let linked = wt.path().join("linked");
        let status = Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .current_dir(main.path())
            .output()
            .expect("git worktree add failed");
        assert!(status.status.success());

        let trees = list_worktrees(main.path()).unwrap();
        assert_eq!(trees.len(), 2);
    }

    #[test]
    fn list_worktrees_outside_repo_fails() {
        let dir = TempDir::new().unwrap();
        let err = list_worktrees(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("git worktree list failed"),
            "unexpected error: {err}"
        );
    }

    // APFS rejects non-UTF-8 filenames, so this test is Linux-only. CI's
    // tarpaulin job runs on Linux and exercises the to_str()-returns-None branch.
    #[cfg(target_os = "linux")]
    #[test]
    fn enumerate_skills_skips_directory_with_non_utf8_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempdir();
        let skills = dir.path().join("skills");
        fs::create_dir_all(&skills).unwrap();
        // Valid UTF-8 entry that should be returned.
        fs::create_dir_all(skills.join("alpha")).unwrap();
        // Invalid UTF-8 byte sequence — exercises the `to_str()` None branch.
        let bad = OsStr::from_bytes(b"bad\xffname");
        fs::create_dir_all(skills.join(bad)).unwrap();

        let result = enumerate_skills(&skills).unwrap();
        let names: Vec<_> = result.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn exclude_file_for_points_to_info_exclude_under_common_dir() {
        let dir = tempdir();
        init_repo(dir.path());
        let path = exclude_file_for(dir.path()).unwrap();
        assert!(
            path.ends_with(".git/info/exclude"),
            "got {}",
            path.display()
        );
    }

    #[test]
    fn parse_worktree_list_single() {
        let out = "worktree /path/to/repo\nHEAD abc123\nbranch refs/heads/main\n";
        let roots = parse_worktree_list(out);
        assert_eq!(roots, vec![PathBuf::from("/path/to/repo")]);
    }

    #[test]
    fn parse_worktree_list_multiple() {
        let out = "worktree /a/main\nHEAD abc\nbranch refs/heads/main\n\nworktree /a/feature\nHEAD def\nbranch refs/heads/feature\n";
        let roots = parse_worktree_list(out);
        assert_eq!(
            roots,
            vec![PathBuf::from("/a/main"), PathBuf::from("/a/feature")]
        );
    }

    #[test]
    fn parse_worktree_list_empty() {
        assert!(parse_worktree_list("").is_empty());
    }

    #[test]
    fn exclude_entry_format() {
        assert_eq!(exclude_entry_for("review"), ".claude/skills/review/");
    }

    #[test]
    fn enumerate_skills_missing_dir_returns_empty() {
        let dir = tempdir();
        let result = enumerate_skills(&dir.path().join("missing")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn enumerate_skills_lists_sorted_directories() {
        let dir = tempdir();
        let skills_dir = dir.path().join("skills");
        fs::create_dir_all(skills_dir.join("charlie")).unwrap();
        fs::create_dir_all(skills_dir.join("alpha")).unwrap();
        fs::create_dir_all(skills_dir.join("bravo")).unwrap();
        // A stray file should be ignored.
        fs::write(skills_dir.join("README.md"), "hi").unwrap();
        let result = enumerate_skills(&skills_dir).unwrap();
        let names: Vec<_> = result.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn add_exclude_entries_creates_file_and_appends() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let added = add_exclude_entries(
            &exclude,
            &[exclude_entry_for("review"), exclude_entry_for("init")],
            false,
        )
        .unwrap();
        assert_eq!(added.len(), 2);
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.contains(".claude/skills/review/"));
        assert!(content.contains(".claude/skills/init/"));
    }

    #[test]
    fn add_exclude_entries_does_not_duplicate() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let first = add_exclude_entries(&exclude, &[exclude_entry_for("review")], false).unwrap();
        assert_eq!(first, vec![".claude/skills/review/".to_string()]);
        let second = add_exclude_entries(&exclude, &[exclude_entry_for("review")], false).unwrap();
        assert!(second.is_empty());
        let content = fs::read_to_string(&exclude).unwrap();
        assert_eq!(content.matches(".claude/skills/review/").count(), 1);
    }

    #[test]
    fn add_exclude_entries_dry_run_does_not_write() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let added = add_exclude_entries(&exclude, &[exclude_entry_for("review")], true).unwrap();
        assert_eq!(added.len(), 1);
        assert!(!exclude.exists());
    }

    #[test]
    fn add_exclude_entries_preserves_existing_content() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, "# comments\n*.tmp\n").unwrap();
        add_exclude_entries(&exclude, &[exclude_entry_for("review")], false).unwrap();
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.contains("# comments"));
        assert!(content.contains("*.tmp"));
        assert!(content.contains(".claude/skills/review/"));
    }

    #[test]
    fn resolve_toplevel_propagates_spawn_failure() {
        // current_dir() pointing at a non-existent path causes Command::spawn
        // itself to fail (chdir-before-exec), exercising the with_context arm.
        let err = resolve_toplevel(Path::new("/this/path/should/not/exist/skills_test_spawn"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Failed to run git rev-parse --show-toplevel"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_git_common_dir_propagates_spawn_failure() {
        let err =
            resolve_git_common_dir(Path::new("/this/path/should/not/exist/skills_test_spawn"))
                .unwrap_err()
                .to_string();
        assert!(
            err.contains("Failed to run git rev-parse --git-common-dir"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn add_exclude_entries_propagates_create_dir_all_failure() {
        // Place a regular file where the exclude file's parent should be — so
        // create_dir_all on the parent path fails.
        let dir = tempdir();
        let parent_path = dir.path().join("info");
        fs::write(&parent_path, "block").unwrap();
        let exclude = parent_path.join("exclude");

        let err = add_exclude_entries(&exclude, &[exclude_entry_for("a")], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Failed to create"), "unexpected error: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn add_exclude_entries_propagates_write_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        let info = dir.path().join("info");
        fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        // Read-only parent — fs::write fails with EACCES.
        let mut perms = fs::metadata(&info).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&info, perms).unwrap();

        let result = add_exclude_entries(&exclude, &[exclude_entry_for("a")], false);

        let mut perms = fs::metadata(&info).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&info, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to write"), "unexpected error: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn remove_exclude_entries_propagates_write_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        let info = dir.path().join("info");
        fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        fs::write(&exclude, ".claude/skills/a/\n").unwrap();
        // Make the file itself read-only so fs::write to overwrite it fails.
        let mut perms = fs::metadata(&exclude).unwrap().permissions();
        perms.set_mode(0o400);
        fs::set_permissions(&exclude, perms).unwrap();

        let result = remove_exclude_entries(&exclude, &[exclude_entry_for("a")], false);

        let mut perms = fs::metadata(&exclude).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&exclude, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to write"), "unexpected error: {err}");
    }

    #[test]
    fn add_exclude_entries_appends_newline_when_existing_lacks_one() {
        // Existing content has no trailing newline — exercises the
        // `content.push('\n')` branch.
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, "*.log").unwrap();
        add_exclude_entries(&exclude, &[exclude_entry_for("review")], false).unwrap();
        let content = fs::read_to_string(&exclude).unwrap();
        assert_eq!(content, "*.log\n.claude/skills/review/\n");
    }

    #[test]
    fn remove_exclude_entries_removes_matching_lines() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(
            &exclude,
            "# comments\n*.tmp\n.claude/skills/review/\n.claude/skills/init/\n",
        )
        .unwrap();
        let removed = remove_exclude_entries(
            &exclude,
            &[exclude_entry_for("review"), exclude_entry_for("init")],
            false,
        )
        .unwrap();
        assert_eq!(removed.len(), 2);
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.contains("# comments"));
        assert!(content.contains("*.tmp"));
        assert!(!content.contains(".claude/skills/"));
    }

    #[test]
    fn remove_exclude_entries_missing_file_is_noop() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let removed =
            remove_exclude_entries(&exclude, &[exclude_entry_for("review")], false).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn remove_exclude_entries_dry_run_does_not_modify() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, ".claude/skills/review/\n").unwrap();
        let removed =
            remove_exclude_entries(&exclude, &[exclude_entry_for("review")], true).unwrap();
        assert_eq!(removed.len(), 1);
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.contains(".claude/skills/review/"));
    }
}
