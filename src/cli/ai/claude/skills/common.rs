//! Shared helpers for skills sync, clean, and status commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

/// Skills directory name relative to a repository or worktree root.
pub(super) const SKILLS_SUBPATH: &str = ".claude/skills";

/// Prefix used for entries written to `.git/info/exclude`.
pub(super) const EXCLUDE_PREFIX: &str = ".claude/skills/";

/// Opening marker for the managed block inside `.git/info/exclude`. Changing
/// this string would orphan blocks written by prior versions — forward
/// compatibility commitment.
pub(super) const BLOCK_BEGIN: &str = "# BEGIN omni-dev-skills (managed — do not edit)";

/// Closing marker for the managed block inside `.git/info/exclude`.
pub(super) const BLOCK_END: &str = "# END omni-dev-skills";

/// Output format shared by `sync`, `clean`, and `status`.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OutputFormat {
    /// Human-readable lines, one per action.
    #[default]
    Text,
    /// Machine-readable YAML document.
    Yaml,
}

/// Runs `git rev-parse --show-toplevel` from `path` and returns the absolute root.
pub(super) fn resolve_toplevel(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()
        .with_context(|| ctx_spawn_failure("git rev-parse --show-toplevel", path))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "git rev-parse --show-toplevel failed in {}: {err}",
            path.display()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("git rev-parse --show-toplevel output was not UTF-8")?;
    Ok(PathBuf::from(stdout.trim()))
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
        .with_context(|| ctx_spawn_failure("git rev-parse --git-common-dir", path))?;
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
    let dir_label = source_skills_dir.display();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("Failed to read directory entry in {dir_label}"))?;
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

/// Upserts the managed skills block inside `.git/info/exclude`.
///
/// Foreign lines outside the block are preserved verbatim. Hand-edits inside
/// the block are not preserved — the block is rewritten with the union of its
/// existing entries and `entries`. Returns the entries newly added.
pub(super) fn upsert_skills_block(
    exclude_file: &Path,
    entries: &[String],
    dry_run: bool,
) -> Result<Vec<String>> {
    let content = read_existing_content(exclude_file)?;
    let lines: Vec<&str> = content.lines().collect();
    let block = find_block(&lines);

    let existing: Vec<String> = match &block {
        Some(b) => lines[b.begin + 1..b.end]
            .iter()
            .filter(|l| **l != BLOCK_BEGIN && **l != BLOCK_END)
            .map(|&s| s.to_string())
            .collect(),
        None => Vec::new(),
    };

    let mut additions: Vec<String> = Vec::new();
    for entry in entries {
        if !existing.iter().any(|e| e == entry) && !additions.iter().any(|e| e == entry) {
            additions.push(entry.clone());
        }
    }

    if additions.is_empty() || dry_run {
        return Ok(additions);
    }

    let mut out_lines: Vec<String> = Vec::new();
    if let Some(b) = block {
        out_lines.extend(lines[..b.begin].iter().map(|&s| s.to_string()));
        out_lines.push(BLOCK_BEGIN.to_string());
        out_lines.extend(existing.iter().cloned());
        out_lines.extend(additions.iter().cloned());
        out_lines.push(BLOCK_END.to_string());
        out_lines.extend(lines[b.end + 1..].iter().map(|&s| s.to_string()));
    } else {
        out_lines.extend(lines.iter().map(|&s| s.to_string()));
        out_lines.push(BLOCK_BEGIN.to_string());
        out_lines.extend(additions.iter().cloned());
        out_lines.push(BLOCK_END.to_string());
    }

    write_exclude_file(exclude_file, &out_lines)?;
    Ok(additions)
}

/// Removes the managed skills block from `.git/info/exclude` entirely.
///
/// Returns every line that was inside the block. Foreign lines outside the
/// block are preserved. No-op (returning empty) if the file or block is absent.
pub(super) fn remove_skills_block(exclude_file: &Path, dry_run: bool) -> Result<Vec<String>> {
    if !exclude_file.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(exclude_file)
        .with_context(|| format!("Failed to read {}", exclude_file.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let Some(block) = find_block(&lines) else {
        return Ok(Vec::new());
    };
    let removed: Vec<String> = lines[block.begin + 1..block.end]
        .iter()
        .map(|&s| s.to_string())
        .collect();

    if dry_run {
        return Ok(removed);
    }

    let mut out_lines: Vec<String> = Vec::new();
    out_lines.extend(lines[..block.begin].iter().map(|&s| s.to_string()));
    out_lines.extend(lines[block.end + 1..].iter().map(|&s| s.to_string()));

    write_exclude_file(exclude_file, &out_lines)?;
    Ok(removed)
}

/// Reads the entries currently inside the managed skills block, if any.
pub(super) fn read_skills_block_entries(exclude_file: &Path) -> Result<Vec<String>> {
    if !exclude_file.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(exclude_file)
        .with_context(|| format!("Failed to read {}", exclude_file.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let Some(block) = find_block(&lines) else {
        return Ok(Vec::new());
    };
    Ok(lines[block.begin + 1..block.end]
        .iter()
        .map(|&s| s.to_string())
        .collect())
}

struct BlockBounds {
    begin: usize,
    end: usize,
}

fn find_block(lines: &[&str]) -> Option<BlockBounds> {
    let begin = lines.iter().position(|l| *l == BLOCK_BEGIN)?;
    let end_offset = lines[begin + 1..].iter().position(|l| *l == BLOCK_END)?;
    Some(BlockBounds {
        begin,
        end: begin + 1 + end_offset,
    })
}

fn read_existing_content(exclude_file: &Path) -> Result<String> {
    if exclude_file.exists() {
        fs::read_to_string(exclude_file)
            .with_context(|| format!("Failed to read {}", exclude_file.display()))
    } else {
        Ok(String::new())
    }
}

fn write_exclude_file(exclude_file: &Path, lines: &[String]) -> Result<()> {
    if let Some(parent) = exclude_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let output = if lines.is_empty() {
        String::new()
    } else {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    };
    fs::write(exclude_file, output)
        .with_context(|| format!("Failed to write {}", exclude_file.display()))
}

fn ctx_spawn_failure(command: &str, path: &Path) -> String {
    format!("Failed to run {command} in {}", path.display())
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
        assert!(common.ends_with(".git"), "got {}", common.display());
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

    #[cfg(target_os = "linux")]
    #[test]
    fn enumerate_skills_skips_directory_with_non_utf8_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempdir();
        let skills = dir.path().join("skills");
        fs::create_dir_all(&skills).unwrap();
        fs::create_dir_all(skills.join("alpha")).unwrap();
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
        fs::write(skills_dir.join("README.md"), "hi").unwrap();
        let result = enumerate_skills(&skills_dir).unwrap();
        let names: Vec<_> = result.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn upsert_skills_block_creates_block_when_absent() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let added = upsert_skills_block(
            &exclude,
            &[exclude_entry_for("review"), exclude_entry_for("init")],
            false,
        )
        .unwrap();
        assert_eq!(
            added,
            vec![
                ".claude/skills/review/".to_string(),
                ".claude/skills/init/".to_string()
            ]
        );
        let content = fs::read_to_string(&exclude).unwrap();
        let expected =
            format!("{BLOCK_BEGIN}\n.claude/skills/review/\n.claude/skills/init/\n{BLOCK_END}\n");
        assert_eq!(content, expected);
    }

    #[test]
    fn upsert_skills_block_appends_to_existing_block() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        upsert_skills_block(&exclude, &[exclude_entry_for("review")], false).unwrap();
        let added = upsert_skills_block(&exclude, &[exclude_entry_for("init")], false).unwrap();
        assert_eq!(added, vec![".claude/skills/init/".to_string()]);
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.contains(".claude/skills/review/"));
        assert!(content.contains(".claude/skills/init/"));
        assert_eq!(content.matches(BLOCK_BEGIN).count(), 1);
        assert_eq!(content.matches(BLOCK_END).count(), 1);
    }

    #[test]
    fn upsert_skills_block_does_not_duplicate() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        upsert_skills_block(&exclude, &[exclude_entry_for("review")], false).unwrap();
        let added = upsert_skills_block(&exclude, &[exclude_entry_for("review")], false).unwrap();
        assert!(added.is_empty());
        let content = fs::read_to_string(&exclude).unwrap();
        assert_eq!(content.matches(".claude/skills/review/").count(), 1);
    }

    #[test]
    fn upsert_skills_block_dry_run_does_not_write() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let added = upsert_skills_block(&exclude, &[exclude_entry_for("review")], true).unwrap();
        assert_eq!(added, vec![".claude/skills/review/".to_string()]);
        assert!(!exclude.exists());
    }

    #[test]
    fn upsert_skills_block_preserves_foreign_lines_before_and_after() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let original = format!(
            "# user comment\n*.tmp\n{BLOCK_BEGIN}\n.claude/skills/review/\n{BLOCK_END}\n*.log\n"
        );
        fs::write(&exclude, &original).unwrap();
        upsert_skills_block(&exclude, &[exclude_entry_for("init")], false).unwrap();
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.starts_with("# user comment\n*.tmp\n"));
        assert!(content.ends_with("*.log\n"));
        assert!(content.contains(".claude/skills/review/"));
        assert!(content.contains(".claude/skills/init/"));
    }

    #[test]
    fn upsert_skills_block_returns_empty_when_input_empty() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let added = upsert_skills_block(&exclude, &[], false).unwrap();
        assert!(added.is_empty());
        assert!(!exclude.exists());
    }

    #[test]
    fn upsert_skills_block_dedupes_within_input() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let added = upsert_skills_block(
            &exclude,
            &[exclude_entry_for("review"), exclude_entry_for("review")],
            false,
        )
        .unwrap();
        assert_eq!(added.len(), 1);
        let content = fs::read_to_string(&exclude).unwrap();
        assert_eq!(content.matches(".claude/skills/review/").count(), 1);
    }

    #[test]
    fn upsert_skills_block_appends_after_foreign_lines_in_new_file() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, "*.log\n").unwrap();
        upsert_skills_block(&exclude, &[exclude_entry_for("review")], false).unwrap();
        let content = fs::read_to_string(&exclude).unwrap();
        assert!(content.starts_with("*.log\n"));
        assert!(content.contains(BLOCK_BEGIN));
        assert!(content.ends_with(&format!("{BLOCK_END}\n")));
    }

    #[test]
    fn upsert_skills_block_propagates_create_dir_all_failure() {
        let dir = tempdir();
        let parent_path = dir.path().join("info");
        fs::write(&parent_path, "block").unwrap();
        let exclude = parent_path.join("exclude");
        let err = upsert_skills_block(&exclude, &[exclude_entry_for("a")], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Failed to create"), "unexpected error: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn upsert_skills_block_propagates_write_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        let info = dir.path().join("info");
        fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        let mut perms = fs::metadata(&info).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&info, perms).unwrap();

        let result = upsert_skills_block(&exclude, &[exclude_entry_for("a")], false);

        let mut perms = fs::metadata(&info).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&info, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to write"), "unexpected error: {err}");
    }

    #[test]
    fn remove_skills_block_removes_block_and_reports_entries() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let content = format!(
            "# comment\n*.tmp\n{BLOCK_BEGIN}\n.claude/skills/review/\n.claude/skills/init/\n{BLOCK_END}\n"
        );
        fs::write(&exclude, &content).unwrap();

        let removed = remove_skills_block(&exclude, false).unwrap();
        assert_eq!(
            removed,
            vec![
                ".claude/skills/review/".to_string(),
                ".claude/skills/init/".to_string()
            ]
        );
        let new_content = fs::read_to_string(&exclude).unwrap();
        assert_eq!(new_content, "# comment\n*.tmp\n");
    }

    #[test]
    fn remove_skills_block_missing_file_is_noop() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let removed = remove_skills_block(&exclude, false).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn remove_skills_block_missing_block_is_noop() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, "# comment\n*.tmp\n").unwrap();
        let removed = remove_skills_block(&exclude, false).unwrap();
        assert!(removed.is_empty());
        let content = fs::read_to_string(&exclude).unwrap();
        assert_eq!(content, "# comment\n*.tmp\n");
    }

    #[test]
    fn remove_skills_block_dry_run_does_not_modify() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let content = format!("{BLOCK_BEGIN}\n.claude/skills/review/\n{BLOCK_END}\n");
        fs::write(&exclude, &content).unwrap();
        let removed = remove_skills_block(&exclude, true).unwrap();
        assert_eq!(removed, vec![".claude/skills/review/".to_string()]);
        assert_eq!(fs::read_to_string(&exclude).unwrap(), content);
    }

    #[test]
    fn remove_skills_block_empties_file_when_only_block_present() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let content = format!("{BLOCK_BEGIN}\n.claude/skills/review/\n{BLOCK_END}\n");
        fs::write(&exclude, &content).unwrap();
        remove_skills_block(&exclude, false).unwrap();
        let new_content = fs::read_to_string(&exclude).unwrap();
        assert_eq!(new_content, "");
    }

    #[cfg(unix)]
    #[test]
    fn remove_skills_block_propagates_write_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        let info = dir.path().join("info");
        fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        fs::write(
            &exclude,
            format!("{BLOCK_BEGIN}\n.claude/skills/a/\n{BLOCK_END}\n"),
        )
        .unwrap();
        let mut perms = fs::metadata(&exclude).unwrap().permissions();
        perms.set_mode(0o400);
        fs::set_permissions(&exclude, perms).unwrap();

        let result = remove_skills_block(&exclude, false);

        let mut perms = fs::metadata(&exclude).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&exclude, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to write"), "unexpected error: {err}");
    }

    #[test]
    fn read_skills_block_entries_returns_entries() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let content = format!(
            "# comment\n{BLOCK_BEGIN}\n.claude/skills/alpha/\n.claude/skills/bravo/\n{BLOCK_END}\n"
        );
        fs::write(&exclude, content).unwrap();
        let entries = read_skills_block_entries(&exclude).unwrap();
        assert_eq!(
            entries,
            vec![
                ".claude/skills/alpha/".to_string(),
                ".claude/skills/bravo/".to_string()
            ]
        );
    }

    #[test]
    fn read_skills_block_entries_missing_file_returns_empty() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        let entries = read_skills_block_entries(&exclude).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn read_skills_block_entries_missing_block_returns_empty() {
        let dir = tempdir();
        let exclude = dir.path().join("info").join("exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, "*.log\n").unwrap();
        let entries = read_skills_block_entries(&exclude).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn find_block_requires_both_markers() {
        let only_begin = vec![BLOCK_BEGIN, ".claude/skills/a/"];
        assert!(find_block(&only_begin).is_none());
        let only_end = vec!["foo", BLOCK_END];
        assert!(find_block(&only_end).is_none());
    }

    #[test]
    fn find_block_returns_none_for_reversed_markers() {
        let reversed = vec![BLOCK_END, "middle", BLOCK_BEGIN];
        assert!(find_block(&reversed).is_none());
    }

    #[test]
    fn resolve_toplevel_propagates_spawn_failure() {
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
}
