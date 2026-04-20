//! Clean command — removes symlinks and the managed exclude block created by `sync`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use super::common::{
    exclude_file_for, list_worktrees, remove_skills_block, resolve_toplevel, OutputFormat,
    SKILLS_SUBPATH,
};

/// Removes skill symlinks and the managed exclude block from one or more targets.
#[derive(Parser)]
pub struct CleanCommand {
    /// Target repository or worktree to clean. Defaults to the current working directory.
    #[arg(long, value_name = "PATH")]
    pub target: Option<PathBuf>,

    /// Also clean every worktree belonging to the target repository.
    #[arg(long)]
    pub worktrees: bool,

    /// Preview the changes without deleting symlinks or modifying the exclude file.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub(super) format: OutputFormat,
}

impl CleanCommand {
    /// Executes the clean command.
    pub fn execute(self) -> Result<()> {
        let cwd = std::env::current_dir().context("Failed to determine current directory")?;
        let target_seed = self.target.clone().unwrap_or(cwd);
        let target_root = resolve_toplevel(&target_seed)?;

        let mut targets = vec![target_root.clone()];
        if self.worktrees {
            for wt in list_worktrees(&target_root)? {
                if !targets.iter().any(|t| t == &wt) {
                    targets.push(wt);
                }
            }
        }

        let report = run_clean(&targets, self.dry_run)?;
        print_report(&report, self.dry_run, self.format)?;
        Ok(())
    }
}

/// Outcome of a clean run.
#[derive(Debug, Default, Serialize)]
pub(super) struct CleanReport {
    pub actions: Vec<CleanAction>,
}

/// Individual action produced by the clean operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum CleanAction {
    Unlinked {
        link: PathBuf,
    },
    Preserved {
        path: PathBuf,
        reason: String,
    },
    ExcludeRemoved {
        exclude_file: PathBuf,
        entry: String,
    },
    DirectoryRemoved {
        path: PathBuf,
    },
}

/// Cleans every supplied target, collecting actions into a single report.
pub(super) fn run_clean(targets: &[PathBuf], dry_run: bool) -> Result<CleanReport> {
    let mut report = CleanReport::default();
    for target_root in targets {
        clean_target(target_root, dry_run, &mut report)?;
    }
    Ok(report)
}

fn clean_target(target_root: &Path, dry_run: bool, report: &mut CleanReport) -> Result<()> {
    let skills_dir = target_root.join(SKILLS_SUBPATH);

    if skills_dir.exists() {
        remove_skill_symlinks(&skills_dir, dry_run, report)?;
    }

    let exclude_file = exclude_file_for(target_root)?;
    let removed = remove_skills_block(&exclude_file, dry_run)?;
    for entry in removed {
        report.actions.push(CleanAction::ExcludeRemoved {
            exclude_file: exclude_file.clone(),
            entry,
        });
    }

    if !dry_run && skills_dir.exists() && is_empty_dir(&skills_dir)? {
        fs::remove_dir(&skills_dir)
            .with_context(|| format!("Failed to remove empty {}", skills_dir.display()))?;
        report
            .actions
            .push(CleanAction::DirectoryRemoved { path: skills_dir });
    }

    Ok(())
}

fn remove_skill_symlinks(skills_dir: &Path, dry_run: bool, report: &mut CleanReport) -> Result<()> {
    let entries = fs::read_dir(skills_dir)
        .with_context(|| format!("Failed to read {}", skills_dir.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("Failed to read entry in {}", skills_dir.display()))?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()).is_none() {
            continue;
        }
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to inspect {}", path.display()))?;
        if meta.file_type().is_symlink() {
            if !dry_run {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove symlink {}", path.display()))?;
            }
            report.actions.push(CleanAction::Unlinked { link: path });
        } else {
            let reason = if meta.is_dir() {
                "real directory".to_string()
            } else {
                "real file".to_string()
            };
            report.actions.push(CleanAction::Preserved { path, reason });
        }
    }
    Ok(())
}

fn is_empty_dir(dir: &Path) -> Result<bool> {
    let mut iter =
        fs::read_dir(dir).with_context(|| format!("Failed to read {}", dir.display()))?;
    Ok(iter.next().is_none())
}

#[derive(Serialize)]
struct CleanOutput<'a> {
    dry_run: bool,
    actions: &'a [CleanAction],
}

fn print_report(report: &CleanReport, dry_run: bool, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Text => {
            print_report_text(report, dry_run);
            Ok(())
        }
        OutputFormat::Yaml => {
            let output = CleanOutput {
                dry_run,
                actions: &report.actions,
            };
            let yaml = serde_yaml::to_string(&output)
                .context("Failed to serialize clean report as YAML")?;
            print!("{yaml}");
            Ok(())
        }
    }
}

fn print_report_text(report: &CleanReport, dry_run: bool) {
    let prefix = if dry_run { "[dry-run] " } else { "" };
    for action in &report.actions {
        match action {
            CleanAction::Unlinked { link } => {
                println!("{prefix}unlinked {}", link.display());
            }
            CleanAction::Preserved { path, reason } => {
                println!("{prefix}preserved {} ({reason})", path.display());
            }
            CleanAction::ExcludeRemoved {
                exclude_file,
                entry,
            } => {
                println!(
                    "{prefix}removed exclude entry {} from {}",
                    entry,
                    exclude_file.display()
                );
            }
            CleanAction::DirectoryRemoved { path } => {
                println!("{prefix}removed empty {}", path.display());
            }
        }
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

    fn init_repo(dir: &Path) {
        let status = std::process::Command::new("git")
            .arg("init")
            .arg(dir)
            .output()
            .expect("git init failed to spawn");
        assert!(status.status.success(), "git init failed: {status:?}");
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
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

    fn write_block(exclude: &Path, entries: &[&str]) {
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let mut content = format!("{BLOCK_BEGIN}\n");
        for e in entries {
            content.push_str(e);
            content.push('\n');
        }
        content.push_str(BLOCK_END);
        content.push('\n');
        fs::write(exclude, content).unwrap();
    }

    #[test]
    fn run_clean_removes_symlinks_and_exclude_block() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        init_repo(tgt_tmp.path());
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        symlink(
            &src_tmp.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();
        let exclude_path = tgt_tmp.path().join(".git/info/exclude");
        let pre_content = format!("# comment\n{BLOCK_BEGIN}\n.claude/skills/alpha/\n{BLOCK_END}\n");
        fs::write(&exclude_path, &pre_content).unwrap();

        let report = run_clean(&[tgt_tmp.path().to_path_buf()], false).unwrap();
        let unlinks = report
            .actions
            .iter()
            .filter(|a| matches!(a, CleanAction::Unlinked { .. }))
            .count();
        assert_eq!(unlinks, 1);
        let removed_entries = report
            .actions
            .iter()
            .filter(|a| matches!(a, CleanAction::ExcludeRemoved { .. }))
            .count();
        assert_eq!(removed_entries, 1);
        assert!(!target_skills_dir.join("alpha").exists());
        let content = fs::read_to_string(&exclude_path).unwrap();
        assert!(content.contains("# comment"));
        assert!(!content.contains(BLOCK_BEGIN));
        assert!(!content.contains(".claude/skills/alpha/"));
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, CleanAction::DirectoryRemoved { .. })));
        assert!(!target_skills_dir.exists());
    }

    #[test]
    fn run_clean_preserves_real_files_and_directories() {
        let tgt_tmp = tempdir();
        init_repo(tgt_tmp.path());
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(target_skills_dir.join("keepme")).unwrap();
        fs::write(target_skills_dir.join("keepme").join("SKILL.md"), "# keep").unwrap();

        let report = run_clean(&[tgt_tmp.path().to_path_buf()], false).unwrap();
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, CleanAction::Preserved { .. })));
        assert!(target_skills_dir.join("keepme").join("SKILL.md").exists());
        assert!(target_skills_dir.exists());
    }

    #[test]
    fn run_clean_dry_run_does_not_modify_filesystem() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        init_repo(tgt_tmp.path());
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        symlink(
            &src_tmp.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();
        let exclude_path = tgt_tmp.path().join(".git/info/exclude");
        write_block(&exclude_path, &[".claude/skills/alpha/"]);
        let pre_content = fs::read_to_string(&exclude_path).unwrap();

        let report = run_clean(&[tgt_tmp.path().to_path_buf()], true).unwrap();
        assert!(!report.actions.is_empty());
        assert!(target_skills_dir.join("alpha").exists());
        let content = fs::read_to_string(&exclude_path).unwrap();
        assert_eq!(content, pre_content);
    }

    #[test]
    fn run_clean_missing_skills_dir_still_removes_block() {
        let tgt_tmp = tempdir();
        init_repo(tgt_tmp.path());
        let exclude_path = tgt_tmp.path().join(".git/info/exclude");
        write_block(&exclude_path, &[".claude/skills/alpha/"]);

        let report = run_clean(&[tgt_tmp.path().to_path_buf()], false).unwrap();
        assert!(report
            .actions
            .iter()
            .any(|a| matches!(a, CleanAction::ExcludeRemoved { .. })));
        let content = fs::read_to_string(&exclude_path).unwrap();
        assert!(!content.contains(BLOCK_BEGIN));
    }

    #[test]
    fn run_clean_no_residue_is_noop() {
        let tgt_tmp = tempdir();
        init_repo(tgt_tmp.path());
        let report = run_clean(&[tgt_tmp.path().to_path_buf()], false).unwrap();
        assert!(report.actions.is_empty());
    }

    #[test]
    fn run_clean_preserves_real_file_reports_file_reason() {
        let tgt_tmp = tempdir();
        init_repo(tgt_tmp.path());
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        fs::write(target_skills_dir.join("README.md"), "hello").unwrap();

        let report = run_clean(&[tgt_tmp.path().to_path_buf()], false).unwrap();
        let preserved = report
            .actions
            .iter()
            .find_map(|a| match a {
                CleanAction::Preserved { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("expected Preserved action");
        assert_eq!(preserved, "real file");
    }

    #[test]
    fn execute_cleans_explicit_target() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        init_repo(tgt_tmp.path());
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        symlink(
            &src_tmp.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();
        let exclude_path = tgt_tmp.path().join(".git/info/exclude");
        write_block(&exclude_path, &[".claude/skills/alpha/"]);

        let cmd = CleanCommand {
            target: Some(tgt_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();

        assert!(!target_skills_dir.join("alpha").exists());
        let content = fs::read_to_string(&exclude_path).unwrap();
        assert!(!content.contains(BLOCK_BEGIN));
    }

    #[test]
    fn execute_dry_run_covers_all_action_branches() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        init_repo(tgt_tmp.path());
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        symlink(
            &src_tmp.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();
        fs::write(target_skills_dir.join("keep.txt"), "keep").unwrap();
        let exclude_path = tgt_tmp.path().join(".git/info/exclude");
        write_block(&exclude_path, &[".claude/skills/alpha/"]);

        let cmd = CleanCommand {
            target: Some(tgt_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: true,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
        assert!(target_skills_dir.join("alpha").exists());
        assert!(target_skills_dir.join("keep.txt").exists());
    }

    #[test]
    fn execute_directory_removed_branch() {
        let src_tmp = tempdir();
        let tgt_tmp = tempdir();
        make_source_skills(src_tmp.path(), &["alpha"]);
        init_repo(tgt_tmp.path());
        let target_skills_dir = tgt_tmp.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        symlink(
            &src_tmp.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();

        let cmd = CleanCommand {
            target: Some(tgt_tmp.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
        assert!(!target_skills_dir.exists());
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

    #[cfg(unix)]
    #[test]
    fn run_clean_propagates_remove_file_failure() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempdir();
        let tgt = tempdir();
        make_source_skills(src.path(), &["alpha"]);
        init_repo(tgt.path());
        let target_skills_dir = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        symlink(
            &src.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();
        let mut perms = fs::metadata(&target_skills_dir).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&target_skills_dir, perms).unwrap();

        let result = run_clean(&[tgt.path().to_path_buf()], false);

        let mut perms = fs::metadata(&target_skills_dir).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&target_skills_dir, perms).unwrap();

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to remove symlink"),
            "unexpected error: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn run_clean_skips_directory_with_non_utf8_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let tgt = tempdir();
        init_repo(tgt.path());
        let target_skills_dir = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        let bad = OsStr::from_bytes(b"bad\xffname");
        fs::create_dir_all(target_skills_dir.join(bad)).unwrap();

        let report = run_clean(&[tgt.path().to_path_buf()], false).unwrap();
        assert!(
            report
                .actions
                .iter()
                .all(|a| !matches!(a, CleanAction::Unlinked { .. })),
            "expected no Unlinked actions, got {:?}",
            report.actions
        );
    }

    #[test]
    fn execute_with_worktrees_cleans_every_worktree() {
        let src = tempdir();
        let tgt_main = tempdir();
        let wt_parent = tempdir();
        let linked = wt_parent.path().join("linked");

        make_source_skills(src.path(), &["alpha"]);
        init_repo_with_commit(tgt_main.path());

        let add_wt = std::process::Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&linked)
            .current_dir(tgt_main.path())
            .output()
            .unwrap();
        assert!(add_wt.status.success(), "git worktree add: {add_wt:?}");

        for root in [tgt_main.path(), linked.as_path()] {
            let skills_dir = root.join(SKILLS_SUBPATH);
            fs::create_dir_all(&skills_dir).unwrap();
            symlink(
                &src.path().join(SKILLS_SUBPATH).join("alpha"),
                &skills_dir.join("alpha"),
            )
            .unwrap();
        }

        let cmd = CleanCommand {
            target: Some(tgt_main.path().to_path_buf()),
            worktrees: true,
            dry_run: false,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();

        assert!(!tgt_main.path().join(SKILLS_SUBPATH).exists());
        assert!(!linked.join(SKILLS_SUBPATH).exists());
    }

    #[test]
    fn execute_yaml_format_serializes_report() {
        let src = tempdir();
        let tgt = tempdir();
        make_source_skills(src.path(), &["alpha"]);
        init_repo(tgt.path());
        let target_skills_dir = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&target_skills_dir).unwrap();
        symlink(
            &src.path().join(SKILLS_SUBPATH).join("alpha"),
            &target_skills_dir.join("alpha"),
        )
        .unwrap();

        let cmd = CleanCommand {
            target: Some(tgt.path().to_path_buf()),
            worktrees: false,
            dry_run: false,
            format: OutputFormat::Yaml,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn print_report_text_covers_all_action_variants_and_both_prefixes() {
        let report = CleanReport {
            actions: vec![
                CleanAction::Unlinked {
                    link: PathBuf::from("/a"),
                },
                CleanAction::Preserved {
                    path: PathBuf::from("/b"),
                    reason: "real file".to_string(),
                },
                CleanAction::ExcludeRemoved {
                    exclude_file: PathBuf::from("/c/.git/info/exclude"),
                    entry: ".claude/skills/x/".to_string(),
                },
                CleanAction::DirectoryRemoved {
                    path: PathBuf::from("/d"),
                },
            ],
        };
        print_report_text(&report, false);
        print_report_text(&report, true);
    }

    #[test]
    fn print_report_yaml_serializes_all_action_variants() {
        let report = CleanReport {
            actions: vec![
                CleanAction::Unlinked {
                    link: PathBuf::from("/a"),
                },
                CleanAction::Preserved {
                    path: PathBuf::from("/b"),
                    reason: "real file".to_string(),
                },
                CleanAction::ExcludeRemoved {
                    exclude_file: PathBuf::from("/c"),
                    entry: ".claude/skills/x/".to_string(),
                },
                CleanAction::DirectoryRemoved {
                    path: PathBuf::from("/d"),
                },
            ],
        };
        let output = CleanOutput {
            dry_run: false,
            actions: &report.actions,
        };
        let yaml = serde_yaml::to_string(&output).unwrap();
        assert!(yaml.contains("type: unlinked"));
        assert!(yaml.contains("type: preserved"));
        assert!(yaml.contains("type: exclude_removed"));
        assert!(yaml.contains("type: directory_removed"));
    }
}
