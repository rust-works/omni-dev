//! Status command — reports skill symlinks and managed exclude-block residue.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use super::common::{
    exclude_file_for, list_worktrees, read_skills_block_entries, resolve_toplevel, OutputFormat,
    SKILLS_SUBPATH,
};

/// Reports what `sync` has left behind in the current target (and optionally all worktrees).
#[derive(Parser)]
pub struct StatusCommand {
    /// Target repository or worktree to inspect. Defaults to the current working directory.
    #[arg(long, value_name = "PATH")]
    pub target: Option<PathBuf>,

    /// Also inspect every worktree belonging to the target repository.
    #[arg(long)]
    pub worktrees: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub(super) format: OutputFormat,
}

impl StatusCommand {
    /// Executes the status command.
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

        let report = run_status(&targets)?;
        print_report(&report, self.format)?;
        Ok(())
    }
}

/// Outcome of a status run across one or more target roots.
#[derive(Debug, Default, Serialize)]
pub(super) struct StatusReport {
    pub targets: Vec<TargetStatus>,
}

impl StatusReport {
    fn is_empty(&self) -> bool {
        self.targets
            .iter()
            .all(|t| t.symlinks.is_empty() && t.exclude_entries.is_empty())
    }
}

/// Residue found inside a single target root.
#[derive(Debug, Serialize)]
pub(super) struct TargetStatus {
    pub root: PathBuf,
    pub symlinks: Vec<SymlinkInfo>,
    pub exclude_file: PathBuf,
    pub exclude_entries: Vec<String>,
}

/// A single symlink inside a target's `.claude/skills/` directory.
#[derive(Debug, Serialize)]
pub(super) struct SymlinkInfo {
    pub path: PathBuf,
    pub points_to: PathBuf,
}

/// Inspects every supplied target and collects any residue from prior syncs.
pub(super) fn run_status(targets: &[PathBuf]) -> Result<StatusReport> {
    let mut report = StatusReport::default();
    for target_root in targets {
        report.targets.push(collect_target_status(target_root)?);
    }
    Ok(report)
}

fn collect_target_status(target_root: &Path) -> Result<TargetStatus> {
    let skills_dir = target_root.join(SKILLS_SUBPATH);
    let symlinks = if skills_dir.exists() {
        collect_symlinks(&skills_dir)?
    } else {
        Vec::new()
    };
    let exclude_file = exclude_file_for(target_root)?;
    let exclude_entries = read_skills_block_entries(&exclude_file)?;
    Ok(TargetStatus {
        root: target_root.to_path_buf(),
        symlinks,
        exclude_file,
        exclude_entries,
    })
}

fn collect_symlinks(skills_dir: &Path) -> Result<Vec<SymlinkInfo>> {
    let mut out = Vec::new();
    let entries = fs::read_dir(skills_dir)
        .with_context(|| format!("Failed to read {}", skills_dir.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("Failed to read entry in {}", skills_dir.display()))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to inspect {}", path.display()))?;
        if !meta.file_type().is_symlink() {
            continue;
        }
        let points_to = fs::read_link(&path)
            .with_context(|| format!("Failed to read link {}", path.display()))?;
        out.push(SymlinkInfo { path, points_to });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[derive(Serialize)]
struct StatusOutput<'a> {
    targets: &'a [TargetStatus],
}

fn print_report(report: &StatusReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Text => {
            if report.is_empty() {
                return Ok(());
            }
            print_report_text(report);
            Ok(())
        }
        OutputFormat::Yaml => {
            let output = StatusOutput {
                targets: &report.targets,
            };
            let yaml = serde_yaml::to_string(&output)
                .context("Failed to serialize status report as YAML")?;
            print!("{yaml}");
            Ok(())
        }
    }
}

fn print_report_text(report: &StatusReport) {
    let mut first = true;
    for target in &report.targets {
        if target.symlinks.is_empty() && target.exclude_entries.is_empty() {
            continue;
        }
        if !first {
            println!();
        }
        first = false;
        println!("{}", target.root.display());
        if !target.symlinks.is_empty() {
            println!("  symlinks:");
            for link in &target.symlinks {
                println!(
                    "    {} -> {}",
                    link.path.display(),
                    link.points_to.display()
                );
            }
        }
        if !target.exclude_entries.is_empty() {
            println!("  exclude block ({}):", target.exclude_file.display());
            for entry in &target.exclude_entries {
                println!("    {entry}");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use super::super::common::test_git::{init_repo, init_repo_with_commit, worktree_add};
    use super::super::common::{BLOCK_BEGIN, BLOCK_END};

    use tempfile::TempDir;

    fn tempdir() -> TempDir {
        std::fs::create_dir_all("tmp").ok();
        TempDir::new_in("tmp").unwrap()
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
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
    fn run_status_reports_symlinks_and_block_entries() {
        let src = tempdir();
        let tgt = tempdir();
        init_repo(tgt.path());
        let source_skill = src.path().join("alpha");
        fs::create_dir_all(&source_skill).unwrap();
        let skills_dir = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&skills_dir).unwrap();
        symlink(&source_skill, &skills_dir.join("alpha")).unwrap();
        write_block(
            &tgt.path().join(".git/info/exclude"),
            &[".claude/skills/alpha/"],
        );

        let report = run_status(&[tgt.path().to_path_buf()]).unwrap();
        assert_eq!(report.targets.len(), 1);
        let t = &report.targets[0];
        assert_eq!(t.symlinks.len(), 1);
        assert_eq!(t.symlinks[0].path, skills_dir.join("alpha"));
        assert_eq!(t.symlinks[0].points_to, source_skill);
        assert_eq!(t.exclude_entries, vec![".claude/skills/alpha/".to_string()]);
    }

    #[test]
    fn run_status_empty_repo_is_empty() {
        let tgt = tempdir();
        init_repo(tgt.path());
        let report = run_status(&[tgt.path().to_path_buf()]).unwrap();
        assert!(report.is_empty());
        assert_eq!(report.targets.len(), 1);
        assert!(report.targets[0].symlinks.is_empty());
        assert!(report.targets[0].exclude_entries.is_empty());
    }

    #[test]
    fn run_status_skips_real_files_in_skills_dir() {
        let tgt = tempdir();
        init_repo(tgt.path());
        let skills_dir = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("README.md"), "hi").unwrap();
        fs::create_dir_all(skills_dir.join("real-skill")).unwrap();

        let report = run_status(&[tgt.path().to_path_buf()]).unwrap();
        assert!(report.targets[0].symlinks.is_empty());
    }

    #[test]
    fn execute_target_defaults_to_cwd_is_quiet_on_clean_repo() {
        let tgt = tempdir();
        init_repo(tgt.path());
        let cmd = StatusCommand {
            target: Some(tgt.path().to_path_buf()),
            worktrees: false,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn execute_text_output_prints_residue() {
        let src = tempdir();
        let tgt = tempdir();
        init_repo(tgt.path());
        let source_skill = src.path().join("alpha");
        fs::create_dir_all(&source_skill).unwrap();
        let skills_dir = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&skills_dir).unwrap();
        symlink(&source_skill, &skills_dir.join("alpha")).unwrap();
        write_block(
            &tgt.path().join(".git/info/exclude"),
            &[".claude/skills/alpha/"],
        );

        let cmd = StatusCommand {
            target: Some(tgt.path().to_path_buf()),
            worktrees: false,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn execute_yaml_format_serializes_report() {
        let src = tempdir();
        let tgt = tempdir();
        init_repo(tgt.path());
        let source_skill = src.path().join("alpha");
        fs::create_dir_all(&source_skill).unwrap();
        let skills_dir = tgt.path().join(SKILLS_SUBPATH);
        fs::create_dir_all(&skills_dir).unwrap();
        symlink(&source_skill, &skills_dir.join("alpha")).unwrap();
        write_block(
            &tgt.path().join(".git/info/exclude"),
            &[".claude/skills/alpha/"],
        );

        let cmd = StatusCommand {
            target: Some(tgt.path().to_path_buf()),
            worktrees: false,
            format: OutputFormat::Yaml,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn execute_with_worktrees_aggregates_all_worktrees() {
        let tgt_main = tempdir();
        let wt_parent = tempdir();
        let linked = wt_parent.path().join("linked");

        init_repo_with_commit(tgt_main.path());
        worktree_add(tgt_main.path(), &linked);

        let src = tempdir();
        let source_skill = src.path().join("alpha");
        fs::create_dir_all(&source_skill).unwrap();
        for root in [tgt_main.path(), linked.as_path()] {
            let skills_dir = root.join(SKILLS_SUBPATH);
            fs::create_dir_all(&skills_dir).unwrap();
            symlink(&source_skill, &skills_dir.join("alpha")).unwrap();
        }
        write_block(
            &tgt_main.path().join(".git/info/exclude"),
            &[".claude/skills/alpha/"],
        );

        let cmd = StatusCommand {
            target: Some(tgt_main.path().to_path_buf()),
            worktrees: true,
            format: OutputFormat::Text,
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn print_report_yaml_serializes_fields() {
        let report = StatusReport {
            targets: vec![TargetStatus {
                root: PathBuf::from("/repo"),
                symlinks: vec![SymlinkInfo {
                    path: PathBuf::from("/repo/.claude/skills/alpha"),
                    points_to: PathBuf::from("/src/.claude/skills/alpha"),
                }],
                exclude_file: PathBuf::from("/repo/.git/info/exclude"),
                exclude_entries: vec![".claude/skills/alpha/".to_string()],
            }],
        };
        let output = StatusOutput {
            targets: &report.targets,
        };
        let yaml = serde_yaml::to_string(&output).unwrap();
        assert!(yaml.contains("root: /repo"));
        assert!(yaml.contains("points_to:"));
        assert!(yaml.contains(".claude/skills/alpha/"));
    }

    #[test]
    fn print_report_text_covers_multiple_targets_and_mixed_empty() {
        let report = StatusReport {
            targets: vec![
                TargetStatus {
                    root: PathBuf::from("/empty"),
                    symlinks: Vec::new(),
                    exclude_file: PathBuf::from("/empty/.git/info/exclude"),
                    exclude_entries: Vec::new(),
                },
                TargetStatus {
                    root: PathBuf::from("/only-symlinks"),
                    symlinks: vec![SymlinkInfo {
                        path: PathBuf::from("/only-symlinks/.claude/skills/alpha"),
                        points_to: PathBuf::from("/src/alpha"),
                    }],
                    exclude_file: PathBuf::from("/only-symlinks/.git/info/exclude"),
                    exclude_entries: Vec::new(),
                },
                TargetStatus {
                    root: PathBuf::from("/only-entries"),
                    symlinks: Vec::new(),
                    exclude_file: PathBuf::from("/only-entries/.git/info/exclude"),
                    exclude_entries: vec![".claude/skills/alpha/".to_string()],
                },
                TargetStatus {
                    root: PathBuf::from("/both"),
                    symlinks: vec![SymlinkInfo {
                        path: PathBuf::from("/both/.claude/skills/bravo"),
                        points_to: PathBuf::from("/src/bravo"),
                    }],
                    exclude_file: PathBuf::from("/both/.git/info/exclude"),
                    exclude_entries: vec![".claude/skills/bravo/".to_string()],
                },
            ],
        };
        print_report_text(&report);
    }

    #[test]
    fn status_report_is_empty_only_when_all_targets_empty() {
        let empty = StatusReport {
            targets: vec![TargetStatus {
                root: PathBuf::from("/a"),
                symlinks: Vec::new(),
                exclude_file: PathBuf::from("/a/.git/info/exclude"),
                exclude_entries: Vec::new(),
            }],
        };
        assert!(empty.is_empty());

        let not_empty = StatusReport {
            targets: vec![TargetStatus {
                root: PathBuf::from("/a"),
                symlinks: Vec::new(),
                exclude_file: PathBuf::from("/a/.git/info/exclude"),
                exclude_entries: vec![".claude/skills/x/".to_string()],
            }],
        };
        assert!(!not_empty.is_empty());
    }
}
