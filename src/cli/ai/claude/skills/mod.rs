//! Claude Code skills management commands.

mod clean;
mod common;
mod status;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand};

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use common::OutputFormat;

    fn tempdir() -> TempDir {
        fs::create_dir_all("tmp").ok();
        TempDir::new_in("tmp").unwrap()
    }

    fn init_repo(dir: &Path) {
        let status = Command::new("git").arg("init").arg(dir).output().unwrap();
        assert!(status.status.success());
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
