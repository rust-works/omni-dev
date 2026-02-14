//! Command template management.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

// Embed the template files as strings
const COMMIT_TWIDDLE_TEMPLATE: &str = include_str!("../templates/commit-twiddle.md");
const PR_CREATE_TEMPLATE: &str = include_str!("../templates/pr-create.md");
const PR_UPDATE_TEMPLATE: &str = include_str!("../templates/pr-update.md");

/// Command template management.
#[derive(Parser)]
pub struct CommandsCommand {
    /// Commands subcommand to execute.
    #[command(subcommand)]
    pub command: CommandsSubcommands,
}

/// Commands subcommands.
#[derive(Subcommand)]
pub enum CommandsSubcommands {
    /// Generates command templates.
    Generate(GenerateCommand),
}

/// Generate command options.
#[derive(Parser)]
pub struct GenerateCommand {
    /// Generate subcommand to execute.
    #[command(subcommand)]
    pub command: GenerateSubcommands,
}

/// Generate subcommands.
#[derive(Subcommand)]
pub enum GenerateSubcommands {
    /// Generates commit-twiddle command template.
    #[command(name = "commit-twiddle")]
    CommitTwiddle,
    /// Generates pr-create command template.
    #[command(name = "pr-create")]
    PrCreate,
    /// Generates pr-update command template.
    #[command(name = "pr-update")]
    PrUpdate,
    /// Generates all command templates.
    All,
}

impl CommandsCommand {
    /// Executes the commands command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            CommandsSubcommands::Generate(generate_cmd) => generate_cmd.execute(),
        }
    }
}

impl GenerateCommand {
    /// Executes the generate command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            GenerateSubcommands::CommitTwiddle => {
                generate_commit_twiddle()?;
                println!("✅ Generated .claude/commands/commit-twiddle.md");
            }
            GenerateSubcommands::PrCreate => {
                generate_pr_create()?;
                println!("✅ Generated .claude/commands/pr-create.md");
            }
            GenerateSubcommands::PrUpdate => {
                generate_pr_update()?;
                println!("✅ Generated .claude/commands/pr-update.md");
            }
            GenerateSubcommands::All => {
                generate_commit_twiddle()?;
                generate_pr_create()?;
                generate_pr_update()?;
                println!("✅ Generated all command templates:");
                println!("   - .claude/commands/commit-twiddle.md");
                println!("   - .claude/commands/pr-create.md");
                println!("   - .claude/commands/pr-update.md");
            }
        }
        Ok(())
    }
}

/// Generates the commit-twiddle command template.
fn generate_commit_twiddle() -> Result<()> {
    ensure_claude_commands_dir()?;
    fs::write(
        ".claude/commands/commit-twiddle.md",
        COMMIT_TWIDDLE_TEMPLATE,
    )
    .context("Failed to write .claude/commands/commit-twiddle.md")?;
    Ok(())
}

/// Generates the pr-create command template.
fn generate_pr_create() -> Result<()> {
    ensure_claude_commands_dir()?;
    fs::write(".claude/commands/pr-create.md", PR_CREATE_TEMPLATE)
        .context("Failed to write .claude/commands/pr-create.md")?;
    Ok(())
}

/// Generates the pr-update command template.
fn generate_pr_update() -> Result<()> {
    ensure_claude_commands_dir()?;
    fs::write(".claude/commands/pr-update.md", PR_UPDATE_TEMPLATE)
        .context("Failed to write .claude/commands/pr-update.md")?;
    Ok(())
}

/// Ensures the .claude/commands directory exists.
fn ensure_claude_commands_dir() -> Result<()> {
    let commands_dir = Path::new(".claude/commands");
    if !commands_dir.exists() {
        fs::create_dir_all(commands_dir).context("Failed to create .claude/commands directory")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_twiddle_template_has_content() {
        assert!(COMMIT_TWIDDLE_TEMPLATE.len() > 10);
    }

    #[test]
    fn pr_create_template_has_content() {
        assert!(PR_CREATE_TEMPLATE.len() > 10);
    }

    #[test]
    fn pr_update_template_has_content() {
        assert!(PR_UPDATE_TEMPLATE.len() > 10);
    }

    #[test]
    fn templates_contain_expected_content() {
        // commit-twiddle template should reference commit messages
        assert!(
            COMMIT_TWIDDLE_TEMPLATE.contains("commit")
                || COMMIT_TWIDDLE_TEMPLATE.contains("twiddle")
        );

        // pr-create template should reference pull request
        assert!(
            PR_CREATE_TEMPLATE.contains("pull request")
                || PR_CREATE_TEMPLATE.contains("PR")
                || PR_CREATE_TEMPLATE.contains("pr")
        );

        // pr-update template should reference update
        assert!(
            PR_UPDATE_TEMPLATE.contains("update")
                || PR_UPDATE_TEMPLATE.contains("PR")
                || PR_UPDATE_TEMPLATE.contains("pr")
        );
    }

    #[test]
    fn templates_are_valid_markdown() {
        // Templates should be valid markdown — basic check: they contain text
        // and don't start with binary content
        assert!(COMMIT_TWIDDLE_TEMPLATE.is_ascii() || COMMIT_TWIDDLE_TEMPLATE.contains('#'));
        assert!(PR_CREATE_TEMPLATE.is_ascii() || PR_CREATE_TEMPLATE.contains('#'));
        assert!(PR_UPDATE_TEMPLATE.is_ascii() || PR_UPDATE_TEMPLATE.contains('#'));
    }

    #[test]
    fn templates_are_distinct() {
        // Each template should be unique
        assert_ne!(COMMIT_TWIDDLE_TEMPLATE, PR_CREATE_TEMPLATE);
        assert_ne!(COMMIT_TWIDDLE_TEMPLATE, PR_UPDATE_TEMPLATE);
        assert_ne!(PR_CREATE_TEMPLATE, PR_UPDATE_TEMPLATE);
    }
}
