//! Sync command — links Claude skills from a source repository into targets.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use super::common::{
    add_exclude_entries, enumerate_skills, exclude_entry_for, exclude_file_for, list_worktrees,
    resolve_toplevel, SKILLS_SUBPATH,
};

/// Syncs Claude skills from a source repository into one or more target worktrees.
#[derive(Parser)]
pub struct SyncCommand {
    /// Source repository or worktree containing the canonical `.claude/skills/`.
    /// Defaults to the current working directory.
    #[arg(long, value_name = "PATH")]
    pub source: Option<PathBuf>,

    /// Target repository or worktree to sync into. Defaults to the source repository.
    #[arg(long, value_name = "PATH")]
    pub target: Option<PathBuf>,

    /// Also sync to every worktree belonging to the target repository.
    #[arg(long)]
    pub worktrees: bool,

    /// Preview the changes without creating symlinks or modifying the exclude file.
    #[arg(long)]
    pub dry_run: bool,
}

impl SyncCommand {
    /// Executes the sync command.
    pub fn execute(self) -> Result<()> {
        let cwd = std::env::current_dir().context("Failed to determine current directory")?;
        let source_seed = self.source.clone().unwrap_or_else(|| cwd.clone());
        let source_root = resolve_toplevel(&source_seed)?;

        let target_seed = self.target.clone().unwrap_or_else(|| source_root.clone());
        let target_root = resolve_toplevel(&target_seed)?;

        let mut targets = vec![target_root.clone()];
        if self.worktrees {
            for wt in list_worktrees(&target_root)? {
                if !targets.iter().any(|t| t == &wt) {
                    targets.push(wt);
                }
            }
        }

        let report = run_sync(&source_root, &targets, self.dry_run)?;
        print_report(&report, self.dry_run);

        if !report.errors.is_empty() {
            anyhow::bail!(
                "{} skill(s) blocked by existing files; see errors above",
                report.errors.len()
            );
        }
        Ok(())
    }
}

/// Outcome of running a sync operation.
#[derive(Debug, Default)]
pub(super) struct SyncReport {
    pub actions: Vec<SyncAction>,
    pub errors: Vec<SyncError>,
}

/// Individual action produced by the sync operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SyncAction {
    Linked {
        link: PathBuf,
        points_to: PathBuf,
    },
    Relinked {
        link: PathBuf,
        points_to: PathBuf,
    },
    Excluded {
        exclude_file: PathBuf,
        entry: String,
    },
    SkippedSameTarget {
        target: PathBuf,
    },
}

/// Error describing a skill that could not be synced because a real file or directory
/// already exists at the target path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SyncError {
    pub target: PathBuf,
    pub reason: String,
}

/// Performs the sync operation for the given source and targets.
pub(super) fn run_sync(
    source_root: &Path,
    targets: &[PathBuf],
    dry_run: bool,
) -> Result<SyncReport> {
    let source_skills_dir = source_root.join(SKILLS_SUBPATH);
    let skills = enumerate_skills(&source_skills_dir)?;
    let mut report = SyncReport::default();

    if skills.is_empty() {
        return Ok(report);
    }

    for target_root in targets {
        if paths_equal(target_root, source_root) {
            report.actions.push(SyncAction::SkippedSameTarget {
                target: target_root.clone(),
            });
            continue;
        }
        sync_to_target(target_root, &skills, dry_run, &mut report)?;
    }

    Ok(report)
}

fn sync_to_target(
    target_root: &Path,
    skills: &[(String, PathBuf)],
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    let target_skills_dir = target_root.join(SKILLS_SUBPATH);
    if !dry_run {
        fs::create_dir_all(&target_skills_dir)
            .with_context(|| format!("Failed to create {}", target_skills_dir.display()))?;
    }

    let mut exclude_entries = Vec::new();
    for (name, source_skill) in skills {
        let link_path = target_skills_dir.join(name);
        match link_skill(&link_path, source_skill, dry_run)? {
            LinkOutcome::Created => {
                report.actions.push(SyncAction::Linked {
                    link: link_path.clone(),
                    points_to: source_skill.clone(),
                });
                exclude_entries.push(exclude_entry_for(name));
            }
            LinkOutcome::Replaced => {
                report.actions.push(SyncAction::Relinked {
                    link: link_path.clone(),
                    points_to: source_skill.clone(),
                });
                exclude_entries.push(exclude_entry_for(name));
            }
            LinkOutcome::Blocked(reason) => {
                report.errors.push(SyncError {
                    target: link_path,
                    reason,
                });
            }
        }
    }

    if !exclude_entries.is_empty() {
        let exclude_file = exclude_file_for(target_root)?;
        let added = add_exclude_entries(&exclude_file, &exclude_entries, dry_run)?;
        for entry in added {
            report.actions.push(SyncAction::Excluded {
                exclude_file: exclude_file.clone(),
                entry,
            });
        }
    }

    Ok(())
}

#[derive(Debug)]
enum LinkOutcome {
    Created,
    Replaced,
    Blocked(String),
}

fn link_skill(link_path: &Path, source_skill: &Path, dry_run: bool) -> Result<LinkOutcome> {
    match fs::symlink_metadata(link_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                if !dry_run {
                    fs::remove_file(link_path).with_context(|| {
                        format!("Failed to remove existing symlink {}", link_path.display())
                    })?;
                    create_symlink(source_skill, link_path).with_context(|| {
                        format!(
                            "Failed to create symlink {} -> {}",
                            link_path.display(),
                            source_skill.display()
                        )
                    })?;
                }
                Ok(LinkOutcome::Replaced)
            } else {
                Ok(LinkOutcome::Blocked(format!(
                    "real {} already exists at {}",
                    if meta.is_dir() { "directory" } else { "file" },
                    link_path.display()
                )))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if !dry_run {
                create_symlink(source_skill, link_path).with_context(|| {
                    format!(
                        "Failed to create symlink {} -> {}",
                        link_path.display(),
                        source_skill.display()
                    )
                })?;
            }
            Ok(LinkOutcome::Created)
        }
        Err(err) => Err(err).with_context(|| format!("Failed to inspect {}", link_path.display())),
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    fs::canonicalize(a)
        .ok()
        .zip(fs::canonicalize(b).ok())
        .map_or_else(|| a == b, |(a, b)| a == b)
}

fn print_report(report: &SyncReport, dry_run: bool) {
    let prefix = if dry_run { "[dry-run] " } else { "" };
    for action in &report.actions {
        match action {
            SyncAction::Linked { link, points_to } => {
                println!(
                    "{prefix}linked {} -> {}",
                    link.display(),
                    points_to.display()
                );
            }
            SyncAction::Relinked { link, points_to } => {
                println!(
                    "{prefix}relinked {} -> {}",
                    link.display(),
                    points_to.display()
                );
            }
            SyncAction::Excluded {
                exclude_file,
                entry,
            } => {
                println!("{prefix}excluded {} in {}", entry, exclude_file.display());
            }
            SyncAction::SkippedSameTarget { target } => {
                println!(
                    "{prefix}skipped {} (target equals source)",
                    target.display()
                );
            }
        }
    }
    for err in &report.errors {
        eprintln!("error: {} -- {}", err.target.display(), err.reason);
    }
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

    fn make_source_skills(root: &Path, names: &[&str]) {
        let skills = root.join(SKILLS_SUBPATH);
        fs::create_dir_all(&skills).unwrap();
        for name in names {
            let skill_dir = skills.join(name);
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(skill_dir.join("SKILL.md"), format!("# {name}")).unwrap();
        }
    }

    /// Initialises a real git repository at `dir` so commands like
    /// `git rev-parse --git-common-dir` work as expected.
    fn init_repo(dir: &Path) {
        let status = std::process::Command::new("git")
            .arg("init")
            .arg(dir)
            .output()
            .expect("git init failed to spawn");
        assert!(status.status.success(), "git init failed: {status:?}");
    }

    fn make_fake_repo(dir: &Path) {
        init_repo(dir);
    }

    #[test]
    fn run_sync_creates_symlinks_and_exclude_entries() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha", "bravo"]);
        make_fake_repo(tgt_tmp.path());

        let targets = vec![tgt_tmp.path().to_path_buf()];
        let report = run_sync(src_tmp.path(), &targets, false).unwrap();
        assert!(report.errors.is_empty());

        for name in ["alpha", "bravo"] {
            let link = tgt_tmp.path().join(SKILLS_SUBPATH).join(name);
            let meta = fs::symlink_metadata(&link).unwrap();
            assert!(meta.file_type().is_symlink(), "{name} should be a symlink");
            let points_to = fs::read_link(&link).unwrap();
            assert_eq!(points_to, src_tmp.path().join(SKILLS_SUBPATH).join(name));
        }

        let exclude = fs::read_to_string(tgt_tmp.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.contains(".claude/skills/alpha/"));
        assert!(exclude.contains(".claude/skills/bravo/"));
    }

    #[test]
    fn run_sync_replaces_existing_symlink() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        let other_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        make_source_skills(other_tmp.path(), &["alpha"]);
        make_fake_repo(tgt_tmp.path());

        // Pre-create a symlink pointing to the "other" source.
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        create_symlink(
            &other_tmp.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();

        let report = run_sync(src_tmp.path(), &[tgt_tmp.path().to_path_buf()], false).unwrap();
        assert!(report.errors.is_empty());
        let relinked = report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Relinked { .. }));
        assert!(relinked, "expected a Relinked action");

        let link = target_skills_dir.join("alpha");
        let points_to = fs::read_link(&link).unwrap();
        assert_eq!(points_to, src_tmp.path().join(SKILLS_SUBPATH).join("alpha"));
    }

    #[test]
    fn run_sync_skips_real_file_and_reports_error() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha", "bravo"]);
        make_fake_repo(tgt_tmp.path());

        // Place a real directory at the target of "alpha".
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(target_skills_dir.join("alpha")).unwrap();
        fs::write(target_skills_dir.join("alpha").join("keep.txt"), "keep").unwrap();

        let report = run_sync(src_tmp.path(), &[tgt_tmp.path().to_path_buf()], false).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].target.ends_with(".claude/skills/alpha"));
        // bravo should still have been linked
        assert!(fs::symlink_metadata(target_skills_dir.join("bravo"))
            .unwrap()
            .file_type()
            .is_symlink());
        // alpha's real file should still exist
        assert!(target_skills_dir.join("alpha").join("keep.txt").exists());
    }

    #[test]
    fn run_sync_dry_run_reports_but_does_not_modify() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        make_fake_repo(tgt_tmp.path());

        let report = run_sync(src_tmp.path(), &[tgt_tmp.path().to_path_buf()], true).unwrap();
        assert!(report.errors.is_empty());
        assert!(!report.actions.is_empty());

        let link = tgt_tmp.path().join(SKILLS_SUBPATH).join("alpha");
        assert!(!link.exists());
        let exclude = fs::read_to_string(tgt_tmp.path().join(".git/info/exclude")).unwrap();
        assert!(!exclude.contains(".claude/skills/alpha/"));
    }

    #[test]
    fn run_sync_does_not_duplicate_exclude_entries() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        make_fake_repo(tgt_tmp.path());

        let targets = vec![tgt_tmp.path().to_path_buf()];
        run_sync(src_tmp.path(), &targets, false).unwrap();
        run_sync(src_tmp.path(), &targets, false).unwrap();

        let exclude = fs::read_to_string(tgt_tmp.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".claude/skills/alpha/").count(), 1);
    }

    #[test]
    fn run_sync_skips_target_equal_to_source() {
        let src_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        make_fake_repo(src_tmp.path());

        let targets = vec![src_tmp.path().to_path_buf()];
        let report = run_sync(src_tmp.path(), &targets, false).unwrap();
        assert!(report.errors.is_empty());
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::SkippedSameTarget { .. })));

        // Original skill directory must not have been touched or replaced.
        let skill = src_tmp.path().join(SKILLS_SUBPATH).join("alpha");
        let meta = fs::symlink_metadata(&skill).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
    }

    #[test]
    fn run_sync_with_no_source_skills_is_noop() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_fake_repo(tgt_tmp.path());

        let targets = vec![tgt_tmp.path().to_path_buf()];
        let report = run_sync(src_tmp.path(), &targets, false).unwrap();
        assert!(report.actions.is_empty());
        assert!(report.errors.is_empty());
        let exclude = fs::read_to_string(tgt_tmp.path().join(".git/info/exclude")).unwrap();
        assert!(!exclude.contains(".claude/skills/"));
    }

    #[test]
    fn paths_equal_returns_true_for_same_canonical_path() {
        let dir = tempdir();
        let a = dir.path().to_path_buf();
        let b = dir.path().to_path_buf();
        assert!(paths_equal(&a, &b));
    }

    #[test]
    fn paths_equal_returns_false_for_different_dirs() {
        let a = tempdir();
        let b = tempdir();
        assert!(!paths_equal(a.path(), b.path()));
    }

    #[test]
    fn paths_equal_falls_back_to_literal_comparison_when_canonicalize_fails() {
        // Neither path exists so canonicalize returns Err — exercises the
        // fallback `|| a == b` branch.
        assert!(paths_equal(
            Path::new("/nonexistent/skills/a"),
            Path::new("/nonexistent/skills/a")
        ));
        assert!(!paths_equal(
            Path::new("/nonexistent/skills/a"),
            Path::new("/nonexistent/skills/b")
        ));
    }

    #[test]
    fn execute_syncs_to_explicit_target() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        init_repo(src_tmp.path());
        init_repo(tgt_tmp.path());
        make_source_skills(src_tmp.path(), &["alpha"]);

        let cmd = SyncCommand {
            source: Some(src_tmp.path().to_path_buf()),
            target: Some(tgt_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
        };
        cmd.execute().unwrap();

        let link = tgt_tmp.path().join(SKILLS_SUBPATH).join("alpha");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn execute_dry_run_covers_all_action_branches() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        init_repo(src_tmp.path());
        init_repo(tgt_tmp.path());
        make_source_skills(src_tmp.path(), &["alpha", "bravo"]);
        // Pre-create a symlink so "alpha" is reported as Relinked.
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        create_symlink(
            &src_tmp.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();

        let cmd = SyncCommand {
            source: Some(src_tmp.path().to_path_buf()),
            target: Some(tgt_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: true,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn execute_returns_error_when_blocked() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        init_repo(src_tmp.path());
        init_repo(tgt_tmp.path());
        make_source_skills(src_tmp.path(), &["alpha"]);
        // Place a real dir so the sync is blocked.
        fs::create_dir_all(tgt_tmp.path().join(SKILLS_SUBPATH).join("alpha")).unwrap();

        let cmd = SyncCommand {
            source: Some(src_tmp.path().to_path_buf()),
            target: Some(tgt_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
        };
        let err = cmd.execute().unwrap_err().to_string();
        assert!(err.contains("blocked by existing files"));
    }

    #[test]
    fn execute_skipped_same_target_covers_print_branch() {
        // Exercises the SkippedSameTarget print path via execute.
        let src_tmp = tempdir();
        init_repo(src_tmp.path());
        make_source_skills(src_tmp.path(), &["alpha"]);

        let cmd = SyncCommand {
            source: Some(src_tmp.path().to_path_buf()),
            target: Some(src_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
        };
        cmd.execute().unwrap();
    }

    /// Initialise a repo with one commit so linked worktrees can be created from it.
    fn init_repo_with_commit(dir: &Path) {
        init_repo(dir);
        fs::write(dir.join("README.md"), "readme").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(add.status.success());
        let commit = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=x@x",
                "-c",
                "user.name=x",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(commit.status.success());
    }

    #[test]
    fn execute_source_defaults_to_cwd() {
        // Covers the `source.unwrap_or_else(|| cwd.clone())` branch. Uses
        // dry_run so no filesystem writes occur on the inferred source repo.
        let tgt = tempdir();
        init_repo(tgt.path());
        let cmd = SyncCommand {
            source: None,
            target: Some(tgt.path().to_path_buf()),
            worktrees: false,
            dry_run: true,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn execute_target_defaults_to_source() {
        // Covers the `target.unwrap_or_else(|| source_root.clone())` branch.
        // With target == source the run skips with SkippedSameTarget, so no
        // filesystem writes occur even though dry_run is false.
        let src = tempdir();
        init_repo(src.path());
        make_source_skills(src.path(), &["alpha"]);

        let cmd = SyncCommand {
            source: Some(src.path().to_path_buf()),
            target: None,
            worktrees: false,
            dry_run: false,
        };
        cmd.execute().unwrap();
        // Source directory must remain a real directory.
        let skill = src.path().join(SKILLS_SUBPATH).join("alpha");
        let meta = fs::symlink_metadata(&skill).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn link_skill_propagates_lstat_failure() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        init_repo(src.path());
        init_repo(tgt.path());
        make_source_skills(src.path(), &["alpha"]);

        let target_skills = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills).unwrap();
        // Removing search permission causes lstat on a path inside this dir to
        // return EACCES (not NotFound), exercising the catch-all error arm.
        let mut perms = fs::metadata(&target_skills).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&target_skills, perms).unwrap();

        let result = run_sync(src.path(), &[tgt.path().to_path_buf()], false);

        let mut perms = fs::metadata(&target_skills).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&target_skills, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to inspect") || err.contains("Failed to create symlink"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn link_skill_propagates_relink_failure_for_existing_symlink() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let other = tempdir();
        let tgt = tempdir();
        init_repo(src.path());
        init_repo(tgt.path());
        make_source_skills(src.path(), &["alpha"]);
        make_source_skills(other.path(), &["alpha"]);

        // Pre-create a symlink at the target then make the parent read-only so
        // the subsequent remove_file/create_symlink fails.
        let target_skills = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills).unwrap();
        create_symlink(
            &other.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills.join("alpha"),
        )
        .unwrap();
        let mut perms = fs::metadata(&target_skills).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&target_skills, perms).unwrap();

        let result = run_sync(src.path(), &[tgt.path().to_path_buf()], false);

        let mut perms = fs::metadata(&target_skills).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&target_skills, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to remove existing symlink")
                || err.contains("Failed to create symlink"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn link_skill_propagates_create_symlink_failure() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        init_repo(src.path());
        init_repo(tgt.path());
        make_source_skills(src.path(), &["alpha"]);

        let target_skills = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills).unwrap();
        // Make the skills directory read-only so create_symlink within it fails.
        let mut perms = fs::metadata(&target_skills).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&target_skills, perms).unwrap();

        let result = run_sync(src.path(), &[tgt.path().to_path_buf()], false);

        // Restore writable perms so TempDir cleanup succeeds.
        let mut perms = fs::metadata(&target_skills).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&target_skills, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to create symlink") || err.contains("Failed to inspect"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn run_sync_propagates_create_dir_all_failure() {
        // Place a regular file at `<target>/.claude` so that creating
        // `<target>/.claude/skills` fails with a context-wrapped error.
        let src = tempdir();
        let tgt = tempdir();
        init_repo(src.path());
        init_repo(tgt.path());
        make_source_skills(src.path(), &["alpha"]);
        fs::write(tgt.path().join(".claude"), "block").unwrap();

        let err = run_sync(src.path(), &[tgt.path().to_path_buf()], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Failed to create"), "unexpected error: {err}");
    }

    #[test]
    fn execute_with_worktrees_syncs_to_all_worktrees() {
        let src = tempdir();
        let tgt_main = tempdir();
        let wt_parent = tempdir();
        let linked = wt_parent.path().join("linked");

        init_repo(src.path());
        make_source_skills(src.path(), &["alpha"]);
        init_repo_with_commit(tgt_main.path());

        let add_wt = std::process::Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .current_dir(tgt_main.path())
            .output()
            .unwrap();
        assert!(add_wt.status.success(), "git worktree add: {add_wt:?}");

        let cmd = SyncCommand {
            source: Some(src.path().to_path_buf()),
            target: Some(tgt_main.path().to_path_buf()),
            worktrees: true,
            dry_run: false,
        };
        cmd.execute().unwrap();

        assert!(
            fs::symlink_metadata(tgt_main.path().join(".claude/skills/alpha"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::symlink_metadata(linked.join(".claude/skills/alpha"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
