//! Configuration-related CLI commands

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Configuration operations
#[derive(Parser)]
pub struct ConfigCommand {
    /// Configuration subcommand to execute
    #[command(subcommand)]
    pub command: ConfigSubcommands,
}

/// Configuration subcommands
#[derive(Subcommand)]
pub enum ConfigSubcommands {
    /// AI model configuration and information
    Models(ModelsCommand),
}

/// Models operations
#[derive(Parser)]
pub struct ModelsCommand {
    /// Models subcommand to execute
    #[command(subcommand)]
    pub command: ModelsSubcommands,
}

/// Models subcommands
#[derive(Subcommand)]
pub enum ModelsSubcommands {
    /// Show the embedded models.yaml configuration
    Show(ShowCommand),
}

/// Show command options
#[derive(Parser)]
pub struct ShowCommand {}

impl ConfigCommand {
    /// Execute config command
    pub fn execute(self) -> Result<()> {
        match self.command {
            ConfigSubcommands::Models(models_cmd) => models_cmd.execute(),
        }
    }
}

impl ModelsCommand {
    /// Execute models command
    pub fn execute(self) -> Result<()> {
        match self.command {
            ModelsSubcommands::Show(show_cmd) => show_cmd.execute(),
        }
    }
}

impl ShowCommand {
    /// Execute show command
    pub fn execute(self) -> Result<()> {
        // Print the embedded models.yaml file
        let yaml_content = include_str!("../templates/models.yaml");
        println!("{}", yaml_content);
        Ok(())
    }
}
