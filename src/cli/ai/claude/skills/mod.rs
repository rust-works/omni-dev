//! Claude Code skills management commands.

mod clean;
mod common;
mod status;
mod sync;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

pub use common::OutputFormat;

/// Output format selector for the MCP `claude_skills_*` tools.
///
/// Mirrors [`OutputFormat`] but lives at the module boundary so callers
/// outside the CLI (e.g. MCP handlers) do not depend on the `clap` re-export.
pub type SkillsFormat = OutputFormat;

/// Worktree-aware distribution of Claude Code skills.
#[derive(Parser)]
pub struct SkillsCommand {
    /// Skills subcommand to execute.
    #[command(subcommand)]
    pub command: SkillsSubcommands,
}

/// Skills subcommands.
#[derive(Subcommand)]
pub enum SkillsSubcommands {
    /// Syncs skills from a source repository into one or more targets.
    Sync(sync::SyncCommand),
    /// Removes skill symlinks and managed exclude block previously created by `sync`.
    Clean(clean::CleanCommand),
    /// Reports residue left by `sync` — symlinks and managed exclude-block entries.
    Status(status::StatusCommand),
}

impl SkillsCommand {
    /// Executes the skills command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            SkillsSubcommands::Sync(cmd) => cmd.execute(),
            SkillsSubcommands::Clean(cmd) => cmd.execute(),
            SkillsSubcommands::Status(cmd) => cmd.execute(),
        }
    }
}

/// Syncs Claude skills and returns the report as a formatted string.
///
/// Non-interactive wrapper around the `sync` subcommand suitable for MCP
/// callers. Source defaults to `base_dir` (or the current working directory
/// when `base_dir` is `None`); target defaults to the source repository.
/// When `worktrees` is true, every worktree belonging to the target
/// repository is synced in addition to the target itself.
///
/// The operation **mutates the filesystem**: creates symlinks inside
/// `.claude/skills/` and upserts a managed block inside
/// `.git/info/exclude`. Always performed as a real (not dry) run — dry-run
/// mode is a CLI convenience, not useful through MCP.
///
/// `base_dir` exists so callers can pass an explicit directory instead of
/// mutating the process-wide cwd. Production MCP callers pass `None`.
pub fn run_sync(
    base_dir: Option<&std::path::Path>,
    worktrees: bool,
    format: OutputFormat,
) -> Result<String> {
    let base = resolve_base_dir(base_dir)?;
    let source_root = common::resolve_toplevel(&base)?;
    let target_root = source_root.clone();

    let mut targets = vec![target_root.clone()];
    if worktrees {
        for wt in common::list_worktrees(&target_root)? {
            if !targets.iter().any(|t| t == &wt) {
                targets.push(wt);
            }
        }
    }

    let report = sync::run_sync(&source_root, &targets, false)?;
    render_sync_report(&report, format)
}

/// Cleans Claude skill residue and returns the report as a formatted string.
///
/// Target defaults to `base_dir` (or cwd when `None`). Mutates the filesystem.
pub fn run_clean(
    base_dir: Option<&std::path::Path>,
    worktrees: bool,
    format: OutputFormat,
) -> Result<String> {
    let base = resolve_base_dir(base_dir)?;
    let target_root = common::resolve_toplevel(&base)?;

    let mut targets = vec![target_root.clone()];
    if worktrees {
        for wt in common::list_worktrees(&target_root)? {
            if !targets.iter().any(|t| t == &wt) {
                targets.push(wt);
            }
        }
    }

    let report = clean::run_clean(&targets, false)?;
    render_clean_report(&report, format)
}

/// Reports Claude skill residue.
///
/// Target defaults to `base_dir` (or cwd when `None`). Read-only.
pub fn run_status(
    base_dir: Option<&std::path::Path>,
    worktrees: bool,
    format: OutputFormat,
) -> Result<String> {
    let base = resolve_base_dir(base_dir)?;
    let target_root = common::resolve_toplevel(&base)?;

    let mut targets = vec![target_root.clone()];
    if worktrees {
        for wt in common::list_worktrees(&target_root)? {
            if !targets.iter().any(|t| t == &wt) {
                targets.push(wt);
            }
        }
    }

    let report = status::run_status(&targets)?;
    render_status_report(&report, format)
}

fn resolve_base_dir(base_dir: Option<&std::path::Path>) -> Result<std::path::PathBuf> {
    match base_dir {
        Some(p) => Ok(p.to_path_buf()),
        None => std::env::current_dir().context("Failed to determine current directory"),
    }
}

#[derive(Serialize)]
struct SyncOutput<'a> {
    dry_run: bool,
    actions: &'a [sync::SyncAction],
    errors: &'a [sync::SyncError],
}

#[derive(Serialize)]
struct CleanOutput<'a> {
    dry_run: bool,
    actions: &'a [clean::CleanAction],
}

#[derive(Serialize)]
struct StatusOutput<'a> {
    targets: &'a [status::TargetStatus],
}

fn render_sync_report(report: &sync::SyncReport, format: OutputFormat) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(render_sync_text(report)),
        OutputFormat::Yaml => {
            let output = SyncOutput {
                dry_run: false,
                actions: &report.actions,
                errors: &report.errors,
            };
            serde_yaml::to_string(&output).context("Failed to serialize sync report as YAML")
        }
    }
}

fn render_sync_text(report: &sync::SyncReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for action in &report.actions {
        match action {
            sync::SyncAction::Linked { link, points_to } => {
                let _ = writeln!(out, "linked {} -> {}", link.display(), points_to.display());
            }
            sync::SyncAction::Relinked { link, points_to } => {
                let _ = writeln!(
                    out,
                    "relinked {} -> {}",
                    link.display(),
                    points_to.display()
                );
            }
            sync::SyncAction::Excluded {
                exclude_file,
                entry,
            } => {
                let _ = writeln!(out, "excluded {} in {}", entry, exclude_file.display());
            }
            sync::SyncAction::SkippedSameTarget { target } => {
                let _ = writeln!(out, "skipped {} (target equals source)", target.display());
            }
        }
    }
    for err in &report.errors {
        let _ = writeln!(out, "error: {} -- {}", err.target.display(), err.reason);
    }
    out
}

fn render_clean_report(report: &clean::CleanReport, format: OutputFormat) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(render_clean_text(report)),
        OutputFormat::Yaml => {
            let output = CleanOutput {
                dry_run: false,
                actions: &report.actions,
            };
            serde_yaml::to_string(&output).context("Failed to serialize clean report as YAML")
        }
    }
}

fn render_clean_text(report: &clean::CleanReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for action in &report.actions {
        match action {
            clean::CleanAction::Unlinked { link } => {
                let _ = writeln!(out, "unlinked {}", link.display());
            }
            clean::CleanAction::Preserved { path, reason } => {
                let _ = writeln!(out, "preserved {} ({reason})", path.display());
            }
            clean::CleanAction::ExcludeRemoved {
                exclude_file,
                entry,
            } => {
                let _ = writeln!(
                    out,
                    "removed exclude entry {} from {}",
                    entry,
                    exclude_file.display()
                );
            }
            clean::CleanAction::DirectoryRemoved { path } => {
                let _ = writeln!(out, "removed empty {}", path.display());
            }
        }
    }
    out
}

fn render_status_report(report: &status::StatusReport, format: OutputFormat) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(render_status_text(report)),
        OutputFormat::Yaml => {
            let output = StatusOutput {
                targets: &report.targets,
            };
            serde_yaml::to_string(&output).context("Failed to serialize status report as YAML")
        }
    }
}

fn render_status_text(report: &status::StatusReport) -> String {
    report
        .targets
        .iter()
        .filter(|t| !(t.symlinks.is_empty() && t.exclude_entries.is_empty()))
        .map(render_status_target)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_status_target(target: &status::TargetStatus) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{}", target.root.display());
    if !target.symlinks.is_empty() {
        out.push_str("  symlinks:\n");
        for link in &target.symlinks {
            let _ = writeln!(
                out,
                "    {} -> {}",
                link.path.display(),
                link.points_to.display()
            );
        }
    }
    if !target.exclude_entries.is_empty() {
        let _ = writeln!(out, "  exclude block ({}):", target.exclude_file.display());
        for entry in &target.exclude_entries {
            let _ = writeln!(out, "    {entry}");
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod skills_api_tests {
    use super::*;

    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::common::test_git::{init_repo, init_repo_with_commit, worktree_add};

    fn tempdir() -> TempDir {
        // Anchor tmp at an absolute path so concurrent chdir-ing tests can't
        // cause a relative "tmp" to resolve inside someone else's tempdir.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        fs::create_dir_all(&root).ok();
        TempDir::new_in(&root).unwrap()
    }

    fn make_source_skills(root: &Path, names: &[&str]) {
        let dir = root.join(".claude/skills");
        fs::create_dir_all(&dir).unwrap();
        for n in names {
            let d = dir.join(n);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("SKILL.md"), format!("# {n}")).unwrap();
        }
    }

    #[test]
    fn run_sync_mcp_with_worktrees_links_skills_and_returns_yaml() {
        let src = tempdir();
        let wt_parent = tempdir();
        let linked = wt_parent.path().join("linked");
        init_repo_with_commit(src.path());
        make_source_skills(src.path(), &["alpha"]);
        worktree_add(src.path(), &linked);

        let out = run_sync(Some(&linked), true, OutputFormat::Yaml).unwrap();
        assert!(out.contains("dry_run: false"), "missing dry_run: {out}");
        assert!(out.contains("actions:"), "missing actions: {out}");
    }

    #[test]
    fn run_sync_mcp_same_source_target_reports_skipped() {
        let src = tempdir();
        init_repo(src.path());
        make_source_skills(src.path(), &["alpha"]);
        let out = run_sync(Some(src.path()), false, OutputFormat::Text).unwrap();
        assert!(out.contains("skipped"), "expected skipped action: {out}");
    }

    #[test]
    fn run_clean_mcp_empty_repo_returns_empty_string_text() {
        let tgt = tempdir();
        init_repo(tgt.path());
        let out = run_clean(Some(tgt.path()), false, OutputFormat::Text).unwrap();
        assert!(out.is_empty(), "expected no actions, got: {out}");
    }

    #[test]
    fn run_clean_mcp_yaml_reports_empty_actions() {
        let tgt = tempdir();
        init_repo(tgt.path());
        let out = run_clean(Some(tgt.path()), false, OutputFormat::Yaml).unwrap();
        assert!(out.contains("dry_run: false"), "missing dry_run: {out}");
        assert!(out.contains("actions:"), "missing actions: {out}");
    }

    #[test]
    fn run_clean_mcp_with_worktrees_covers_all_worktrees() {
        let main = tempdir();
        init_repo_with_commit(main.path());
        let wt_parent = tempdir();
        let linked = wt_parent.path().join("linked");
        worktree_add(main.path(), &linked);

        let out = run_clean(Some(main.path()), true, OutputFormat::Yaml).unwrap();
        assert!(out.contains("actions:"), "missing actions: {out}");
    }

    #[test]
    fn run_status_mcp_empty_repo_text_is_empty() {
        let tgt = tempdir();
        init_repo(tgt.path());
        let out = run_status(Some(tgt.path()), false, OutputFormat::Text).unwrap();
        assert!(out.is_empty(), "expected no residue, got: {out}");
    }

    #[test]
    fn run_status_mcp_yaml_emits_targets_array() {
        let tgt = tempdir();
        init_repo(tgt.path());
        let out = run_status(Some(tgt.path()), false, OutputFormat::Yaml).unwrap();
        assert!(out.contains("targets:"), "missing targets: {out}");
    }

    #[cfg(unix)]
    #[test]
    fn run_status_mcp_reports_symlinks_in_text() {
        let src = tempdir();
        let tgt = tempdir();
        init_repo(tgt.path());
        let src_skill = src.path().join("alpha");
        fs::create_dir_all(&src_skill).unwrap();
        let skills_dir = tgt.path().join(".claude/skills");
        fs::create_dir_all(&skills_dir).unwrap();
        std::os::unix::fs::symlink(&src_skill, skills_dir.join("alpha")).unwrap();

        let out = run_status(Some(tgt.path()), false, OutputFormat::Text).unwrap();
        assert!(out.contains(".claude/skills/alpha"), "got: {out}");
    }

    #[test]
    fn run_status_mcp_includes_worktrees_when_requested() {
        let tgt_main = tempdir();
        let wt_parent = tempdir();
        let linked = wt_parent.path().join("linked");
        init_repo_with_commit(tgt_main.path());
        worktree_add(tgt_main.path(), &linked);

        let out = run_status(Some(tgt_main.path()), true, OutputFormat::Yaml).unwrap();
        assert!(out.contains("targets:"), "missing targets: {out}");
    }

    #[test]
    fn run_sync_mcp_errors_outside_repo() {
        let plain = TempDir::new().unwrap();
        let err = run_sync(Some(plain.path()), false, OutputFormat::Text).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("git rev-parse --show-toplevel failed")
                || msg.contains("Failed to run git"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn run_clean_mcp_errors_outside_repo() {
        let plain = TempDir::new().unwrap();
        let err = run_clean(Some(plain.path()), false, OutputFormat::Text).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("git rev-parse"), "unexpected: {msg}");
    }

    #[test]
    fn run_status_mcp_errors_outside_repo() {
        let plain = TempDir::new().unwrap();
        let err = run_status(Some(plain.path()), false, OutputFormat::Text).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("git rev-parse"), "unexpected: {msg}");
    }

    #[test]
    fn run_status_mcp_defaults_base_dir_to_cwd() {
        // Just verify None-path resolves to cwd: cwd may or may not be a repo,
        // but this at least exercises resolve_base_dir's None branch.
        let _ = run_status(None, false, OutputFormat::Text);
    }

    #[test]
    fn render_sync_text_covers_all_variants() {
        let report = sync::SyncReport {
            actions: vec![
                sync::SyncAction::Linked {
                    link: PathBuf::from("/a"),
                    points_to: PathBuf::from("/b"),
                },
                sync::SyncAction::Relinked {
                    link: PathBuf::from("/c"),
                    points_to: PathBuf::from("/d"),
                },
                sync::SyncAction::Excluded {
                    exclude_file: PathBuf::from("/e"),
                    entry: ".claude/skills/x/".into(),
                },
                sync::SyncAction::SkippedSameTarget {
                    target: PathBuf::from("/f"),
                },
            ],
            errors: vec![sync::SyncError {
                target: PathBuf::from("/g"),
                reason: "blocked".into(),
            }],
        };
        let out = render_sync_text(&report);
        for s in [
            "linked /a -> /b",
            "relinked /c -> /d",
            "excluded .claude/skills/x/ in /e",
            "skipped /f",
            "error: /g",
        ] {
            assert!(out.contains(s), "missing {s}: {out}");
        }
    }

    #[test]
    fn render_clean_text_covers_all_variants() {
        let report = clean::CleanReport {
            actions: vec![
                clean::CleanAction::Unlinked {
                    link: PathBuf::from("/a"),
                },
                clean::CleanAction::Preserved {
                    path: PathBuf::from("/b"),
                    reason: "real file".into(),
                },
                clean::CleanAction::ExcludeRemoved {
                    exclude_file: PathBuf::from("/c"),
                    entry: ".claude/skills/x/".into(),
                },
                clean::CleanAction::DirectoryRemoved {
                    path: PathBuf::from("/d"),
                },
            ],
        };
        let out = render_clean_text(&report);
        for s in [
            "unlinked /a",
            "preserved /b (real file)",
            "removed exclude entry .claude/skills/x/ from /c",
            "removed empty /d",
        ] {
            assert!(out.contains(s), "missing {s}: {out}");
        }
    }

    #[test]
    fn render_status_text_covers_mixed_targets() {
        let report = status::StatusReport {
            targets: vec![
                status::TargetStatus {
                    root: PathBuf::from("/empty"),
                    symlinks: Vec::new(),
                    exclude_file: PathBuf::from("/empty/.git/info/exclude"),
                    exclude_entries: Vec::new(),
                },
                status::TargetStatus {
                    root: PathBuf::from("/first"),
                    symlinks: Vec::new(),
                    exclude_file: PathBuf::from("/first/.git/info/exclude"),
                    exclude_entries: vec![".claude/skills/beta/".into()],
                },
                status::TargetStatus {
                    root: PathBuf::from("/both"),
                    symlinks: vec![status::SymlinkInfo {
                        path: PathBuf::from("/both/.claude/skills/alpha"),
                        points_to: PathBuf::from("/src/alpha"),
                    }],
                    exclude_file: PathBuf::from("/both/.git/info/exclude"),
                    exclude_entries: vec![".claude/skills/alpha/".into()],
                },
            ],
        };
        let out = render_status_text(&report);
        assert!(out.contains("/first"), "missing /first: {out}");
        assert!(out.contains("/both"), "missing /both: {out}");
        assert!(out.contains("symlinks:"), "missing symlinks: {out}");
        assert!(
            out.contains("exclude block"),
            "missing exclude block: {out}"
        );
        // Two rendered targets must be separated by a blank line (the join).
        assert!(
            out.contains("\n\n"),
            "missing blank-line separator between targets: {out}"
        );
        assert!(
            !out.contains("/empty\n"),
            "should skip empty target header: {out}"
        );
    }

    #[test]
    fn render_sync_yaml_contains_dry_run_and_actions_keys() {
        let report = sync::SyncReport {
            actions: Vec::new(),
            errors: Vec::new(),
        };
        let out = render_sync_report(&report, OutputFormat::Yaml).unwrap();
        assert!(out.contains("dry_run: false"));
        assert!(out.contains("actions:"));
        assert!(out.contains("errors:"));
    }

    #[test]
    fn render_clean_yaml_contains_dry_run_and_actions_keys() {
        let report = clean::CleanReport {
            actions: Vec::new(),
        };
        let out = render_clean_report(&report, OutputFormat::Yaml).unwrap();
        assert!(out.contains("dry_run: false"));
        assert!(out.contains("actions:"));
    }

    #[test]
    fn render_status_yaml_contains_targets_key() {
        let report = status::StatusReport {
            targets: Vec::new(),
        };
        let out = render_status_report(&report, OutputFormat::Yaml).unwrap();
        assert!(out.contains("targets:"));
    }

    // Prevent clippy from complaining about unused imports in this block.
    #[allow(dead_code)]
    fn _unused(_: PathBuf) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::common::test_git::init_repo;
    use common::OutputFormat;

    fn tempdir() -> TempDir {
        // Anchor tmp at an absolute path so concurrent chdir-ing tests can't
        // cause a relative "tmp" to resolve inside someone else's tempdir.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        fs::create_dir_all(&root).ok();
        TempDir::new_in(&root).unwrap()
    }

    #[test]
    fn dispatch_sync() {
        let src = tempdir();
        let tgt = tempdir();
        init_repo(src.path());
        init_repo(tgt.path());
        let skills_dir = src.path().join(".claude/skills/alpha");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("SKILL.md"), "# alpha").unwrap();

        let cmd = SkillsCommand {
            command: SkillsSubcommands::Sync(sync::SyncCommand {
                source: Some(src.path().to_path_buf()),
                target: Some(tgt.path().to_path_buf()),
                worktrees: false,
                dry_run: false,
                format: OutputFormat::Text,
            }),
        };
        cmd.execute().unwrap();
        assert!(tgt.path().join(".claude/skills/alpha").exists());
    }

    #[test]
    fn dispatch_clean() {
        let tgt = tempdir();
        init_repo(tgt.path());

        let cmd = SkillsCommand {
            command: SkillsSubcommands::Clean(clean::CleanCommand {
                target: Some(tgt.path().to_path_buf()),
                worktrees: false,
                dry_run: false,
                format: OutputFormat::Text,
            }),
        };
        cmd.execute().unwrap();
    }

    #[test]
    fn dispatch_status() {
        let tgt = tempdir();
        init_repo(tgt.path());

        let cmd = SkillsCommand {
            command: SkillsSubcommands::Status(status::StatusCommand {
                target: Some(tgt.path().to_path_buf()),
                worktrees: false,
                format: OutputFormat::Text,
            }),
        };
        cmd.execute().unwrap();
    }
}
