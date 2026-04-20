//! Sync command — links Claude skills from a source repository into targets.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use super::common::{
    enumerate_skills, exclude_entry_for, exclude_file_for, list_worktrees, resolve_toplevel,
    upsert_skills_block, OutputFormat, SKILLS_SUBPATH,
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

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub(super) format: OutputFormat,
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
        print_report(&report, self.dry_run, self.format)?;

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
#[derive(Debug, Default, Serialize)]
pub(super) struct SyncReport {
    pub actions: Vec<SyncAction>,
    pub errors: Vec<SyncError>,
}

/// Individual action produced by the sync operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
        let added = upsert_skills_block(&exclude_file, &exclude_entries, dry_run)?;
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
    let outcome = match fs::symlink_metadata(link_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if !dry_run {
                fs::remove_file(link_path).with_context(|| ctx_remove_symlink(link_path))?;
            }
            LinkOutcome::Replaced
        }
        Ok(meta) => {
            return Ok(LinkOutcome::Blocked(format!(
                "real {} already exists at {}",
                if meta.is_dir() { "directory" } else { "file" },
                link_path.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => LinkOutcome::Created,
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to inspect {}", link_path.display()));
        }
    };
    if !dry_run {
        create_symlink(source_skill, link_path)
            .with_context(|| ctx_create_symlink(link_path, source_skill))?;
    }
    Ok(outcome)
}

fn ctx_remove_symlink(link: &Path) -> String {
    format!("Failed to remove existing symlink {}", link.display())
}

fn ctx_create_symlink(link: &Path, source: &Path) -> String {
    format!(
        "Failed to create symlink {} -> {}",
        link.display(),
        source.display()
    )
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

#[derive(Serialize)]
struct SyncOutput<'a> {
    dry_run: bool,
    actions: &'a [SyncAction],
    errors: &'a [SyncError],
}

fn print_report(report: &SyncReport, dry_run: bool, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Text => {
            print_report_text(report, dry_run);
            Ok(())
        }
        OutputFormat::Yaml => {
            let output = SyncOutput {
                dry_run,
                actions: &report.actions,
                errors: &report.errors,
            };
            let yaml = serde_yaml::to_string(&output)
                .context("Failed to serialize sync report as YAML")?;
            print!("{yaml}");
            Ok(())
        }
    }
}

fn print_report_text(report: &SyncReport, dry_run: bool) {
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

    use super::super::common::{BLOCK_BEGIN, BLOCK_END};

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
    fn run_sync_creates_symlinks_and_exclude_block() {
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
        assert!(exclude.contains(BLOCK_BEGIN));
        assert!(exclude.contains(BLOCK_END));
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

        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(target_skills_dir.join("alpha")).unwrap();
        fs::write(target_skills_dir.join("alpha").join("keep.txt"), "keep").unwrap();

        let report = run_sync(src_tmp.path(), &[tgt_tmp.path().to_path_buf()], false).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].target.ends_with(".claude/skills/alpha"));
        assert!(fs::symlink_metadata(target_skills_dir.join("bravo"))
            .unwrap()
            .file_type()
            .is_symlink());
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
        assert_eq!(exclude.matches(BLOCK_BEGIN).count(), 1);
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
            format: OutputFormat::Text,
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
            format: OutputFormat::Text,
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
        fs::create_dir_all(tgt_tmp.path().join(SKILLS_SUBPATH).join("alpha")).unwrap();

        let cmd = SyncCommand {
            source: Some(src_tmp.path().to_path_buf()),
            target: Some(tgt_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
            format: OutputFormat::Text,
        };
        let err = cmd.execute().unwrap_err().to_string();
        assert!(err.contains("blocked by existing files"));
    }

    #[test]
    fn execute_skipped_same_target_covers_print_branch() {
        let src_tmp = tempdir();
        init_repo(src_tmp.path());
        make_source_skills(src_tmp.path(), &["alpha"]);

        let cmd = SyncCommand {
            source: Some(src_tmp.path().to_path_buf()),
            target: Some(src_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
    }

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
        let tgt = tempdir();
        init_repo(tgt.path());
        let cmd = SyncCommand {
            source: None,
            target: Some(tgt.path().to_path_buf()),
            worktrees: false,
            dry_run: true,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn execute_target_defaults_to_source() {
        let src = tempdir();
        init_repo(src.path());
        make_source_skills(src.path(), &["alpha"]);

        let cmd = SyncCommand {
            source: Some(src.path().to_path_buf()),
            target: None,
            worktrees: false,
            dry_run: false,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
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
        let mut perms = fs::metadata(&target_skills).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&target_skills, perms).unwrap();

        let result = run_sync(src.path(), &[tgt.path().to_path_buf()], false);

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
            format: OutputFormat::Text,
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

    #[test]
    fn execute_yaml_format_serializes_report() {
        let src = tempdir();
        let tgt = tempdir();
        init_repo(src.path());
        init_repo(tgt.path());
        make_source_skills(src.path(), &["alpha"]);

        let cmd = SyncCommand {
            source: Some(src.path().to_path_buf()),
            target: Some(tgt.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
            format: OutputFormat::Yaml,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn print_report_text_covers_all_action_variants_and_both_prefixes() {
        let report = SyncReport {
            actions: vec![
                SyncAction::Linked {
                    link: PathBuf::from("/a"),
                    points_to: PathBuf::from("/b"),
                },
                SyncAction::Relinked {
                    link: PathBuf::from("/c"),
                    points_to: PathBuf::from("/d"),
                },
                SyncAction::Excluded {
                    exclude_file: PathBuf::from("/e"),
                    entry: ".claude/skills/x/".to_string(),
                },
                SyncAction::SkippedSameTarget {
                    target: PathBuf::from("/f"),
                },
            ],
            errors: vec![SyncError {
                target: PathBuf::from("/g"),
                reason: "blocked".to_string(),
            }],
        };
        print_report_text(&report, false);
        print_report_text(&report, true);
    }

    #[test]
    fn print_report_yaml_serializes_all_action_variants() {
        let report = SyncReport {
            actions: vec![
                SyncAction::Linked {
                    link: PathBuf::from("/a"),
                    points_to: PathBuf::from("/b"),
                },
                SyncAction::Relinked {
                    link: PathBuf::from("/c"),
                    points_to: PathBuf::from("/d"),
                },
                SyncAction::Excluded {
                    exclude_file: PathBuf::from("/e"),
                    entry: ".claude/skills/x/".to_string(),
                },
                SyncAction::SkippedSameTarget {
                    target: PathBuf::from("/f"),
                },
            ],
            errors: vec![SyncError {
                target: PathBuf::from("/g"),
                reason: "blocked".to_string(),
            }],
        };
        let output = SyncOutput {
            dry_run: true,
            actions: &report.actions,
            errors: &report.errors,
        };
        let yaml = serde_yaml::to_string(&output).unwrap();
        assert!(yaml.contains("dry_run: true"));
        assert!(yaml.contains("type: linked"));
        assert!(yaml.contains("type: relinked"));
        assert!(yaml.contains("type: excluded"));
        assert!(yaml.contains("type: skipped_same_target"));
        assert!(yaml.contains("blocked"));
    }
}
