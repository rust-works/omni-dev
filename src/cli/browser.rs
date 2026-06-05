//! Browser bridge CLI commands.

pub(crate) mod bridge;
pub(crate) mod harvest;
pub(crate) mod request;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Browser bridge: drive authenticated requests through a browser tab.
#[derive(Parser)]
pub struct BrowserCommand {
    /// The browser subcommand to execute.
    #[command(subcommand)]
    pub command: BrowserSubcommands,
}

/// Browser subcommands.
#[derive(Subcommand)]
pub enum BrowserSubcommands {
    /// Bridge: run the server (`serve`) or send a request through it (`request`).
    Bridge(bridge::BridgeCommand),
}

impl BrowserCommand {
    /// Executes the browser command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            BrowserSubcommands::Bridge(cmd) => cmd.execute().await,
        }
    }
}
