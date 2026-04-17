//! Claude Code diagnostics and inspection commands.

mod cli;
mod skills;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Claude Code diagnostics and inspection.
#[derive(Parser)]
pub struct ClaudeCommand {
    /// Claude subcommand to execute.
    #[command(subcommand)]
    pub command: ClaudeSubcommands,
}

/// Claude subcommands.
#[derive(Subcommand)]
pub enum ClaudeSubcommands {
    /// Claude Code CLI inspection.
    Cli(cli::CliCommand),
    /// Manages Claude Code skills across repositories and worktrees.
    Skills(skills::SkillsCommand),
}

impl ClaudeCommand {
    /// Executes the claude command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            ClaudeSubcommands::Cli(cmd) => cmd.execute(),
            ClaudeSubcommands::Skills(cmd) => cmd.execute(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::process::Command as ProcessCommand;

    use tempfile::TempDir;

    fn tempdir() -> TempDir {
        fs::create_dir_all("tmp").ok();
        TempDir::new_in("tmp").unwrap()
    }

    fn init_repo(dir: &Path) {
        let status = ProcessCommand::new("git")
            .arg("init")
            .arg(dir)
            .output()
            .unwrap();
        assert!(status.status.success());
    }

    #[test]
    fn dispatches_skills_subcommand_via_clap() {
        let src = tempdir();
        init_repo(src.path());
        let cmd = ClaudeCommand::try_parse_from([
            "claude",
            "skills",
            "sync",
            "--source",
            src.path().to_str().unwrap(),
            "--target",
            src.path().to_str().unwrap(),
        ])
        .unwrap();
        cmd.execute().unwrap();
    }

    #[test]
    fn dispatches_cli_subcommand_via_clap() {
        let cmd = ClaudeCommand::try_parse_from(["claude", "cli", "model", "resolve"]).unwrap();
        cmd.execute().unwrap();
    }
}
