//! Amend command — applies commit message amendments from a YAML file.

use anyhow::{Context, Result};
use clap::Parser;

/// Amend command options.
#[derive(Parser)]
pub struct AmendCommand {
    /// YAML file containing commit amendments.
    #[arg(value_name = "YAML_FILE")]
    pub yaml_file: String,

    /// Allows amending commits that already exist in remote main branches (rewrites published history).
    #[arg(long)]
    pub allow_pushed: bool,
}

impl AmendCommand {
    /// Executes the amend command.
    ///
    /// `repo` is the repository location resolved at the CLI boundary
    /// (`None` = current working directory).
    pub fn execute(self, repo: Option<&std::path::Path>) -> Result<()> {
        use crate::git::AmendmentHandler;

        // Resolve the repo root once; the preflight checks and the amendment
        // handler (including all its `git` subprocesses) anchor to it.
        let repo_root = match repo {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("Failed to determine current directory")?,
        };

        // Preflight checks: validate prerequisites before any processing
        crate::utils::check_git_repository_at(&repo_root)?;
        crate::utils::check_working_directory_clean_at(&repo_root)?;

        println!("🔄 Starting commit amendment process...");
        println!("📄 Loading amendments from: {}", self.yaml_file);

        // Create amendment handler and apply amendments
        let handler = AmendmentHandler::new(&repo_root)
            .context("Failed to initialize amendment handler")?
            .with_allow_pushed(self.allow_pushed);

        handler
            .apply_amendments(&self.yaml_file)
            .context("Failed to apply amendments")?;

        Ok(())
    }
}
