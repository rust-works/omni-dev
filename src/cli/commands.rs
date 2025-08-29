//! Command template management

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::Path;

// Embed the template files as strings
const COMMIT_TWIDDLE_TEMPLATE: &str = include_str!("../templates/commit-twiddle.md");
const PR_CREATE_TEMPLATE: &str = include_str!("../templates/pr-create.md");
const PR_UPDATE_TEMPLATE: &str = include_str!("../templates/pr-update.md");

/// Command template management
#[derive(Parser)]
pub struct CommandsCommand {
    /// Commands subcommand to execute
    #[command(subcommand)]
    pub command: CommandsSubcommands,
}

/// Commands subcommands
#[derive(Subcommand)]
pub enum CommandsSubcommands {
    /// Generate command templates
    Generate(GenerateCommand),
}

/// Generate command options
#[derive(Parser)]
pub struct GenerateCommand {
    /// Generate subcommand to execute
    #[command(subcommand)]
    pub command: GenerateSubcommands,
}

/// Generate subcommands
#[derive(Subcommand)]
pub enum GenerateSubcommands {
    /// Generate commit-twiddle command template
    #[command(name = "commit-twiddle")]
    CommitTwiddle,
    /// Generate pr-create command template
    #[command(name = "pr-create")]
    PrCreate,
    /// Generate pr-update command template
    #[command(name = "pr-update")]
    PrUpdate,
    /// Generate all command templates
    All,
}

impl CommandsCommand {
    /// Execute commands command
    pub fn execute(self) -> Result<()> {
        match self.command {
            CommandsSubcommands::Generate(generate_cmd) => generate_cmd.execute(),
        }
    }
}

impl GenerateCommand {
    /// Execute generate command
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

/// Generate commit-twiddle command template
fn generate_commit_twiddle() -> Result<()> {
    ensure_claude_commands_dir()?;
    fs::write(
        ".claude/commands/commit-twiddle.md",
        COMMIT_TWIDDLE_TEMPLATE,
    )
    .context("Failed to write .claude/commands/commit-twiddle.md")?;
    Ok(())
}

/// Generate pr-create command template
fn generate_pr_create() -> Result<()> {
    ensure_claude_commands_dir()?;
    fs::write(".claude/commands/pr-create.md", PR_CREATE_TEMPLATE)
        .context("Failed to write .claude/commands/pr-create.md")?;
    Ok(())
}

/// Generate pr-update command template
fn generate_pr_update() -> Result<()> {
    ensure_claude_commands_dir()?;
    fs::write(".claude/commands/pr-update.md", PR_UPDATE_TEMPLATE)
        .context("Failed to write .claude/commands/pr-update.md")?;
    Ok(())
}

/// Ensure the .claude/commands directory exists
fn ensure_claude_commands_dir() -> Result<()> {
    let commands_dir = Path::new(".claude/commands");
    if !commands_dir.exists() {
        fs::create_dir_all(commands_dir).context("Failed to create .claude/commands directory")?;
    }
    Ok(())
}
