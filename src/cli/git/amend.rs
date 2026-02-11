//! Amend command — applies commit message amendments from a YAML file.

use anyhow::{Context, Result};
use clap::Parser;

/// Amend command options.
#[derive(Parser)]
pub struct AmendCommand {
    /// YAML file containing commit amendments.
    #[arg(value_name = "YAML_FILE")]
    pub yaml_file: String,
}

impl AmendCommand {
    /// Executes the amend command.
    pub fn execute(self) -> Result<()> {
        use crate::git::AmendmentHandler;

        println!("🔄 Starting commit amendment process...");
        println!("📄 Loading amendments from: {}", self.yaml_file);

        // Create amendment handler and apply amendments
        let handler = AmendmentHandler::new().context("Failed to initialize amendment handler")?;

        handler
            .apply_amendments(&self.yaml_file)
            .context("Failed to apply amendments")?;

        Ok(())
    }
}
