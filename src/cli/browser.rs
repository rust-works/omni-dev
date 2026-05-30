//! Browser bridge CLI commands.

pub(crate) mod bridge;
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
    /// Runs the local bridge server (WebSocket + HTTP control planes).
    Bridge(bridge::BridgeCommand),
    /// Sends a request through a running bridge (thin client).
    Request(request::RequestCommand),
}

impl BrowserCommand {
    /// Executes the browser command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            BrowserSubcommands::Bridge(cmd) => cmd.execute().await,
            BrowserSubcommands::Request(cmd) => cmd.execute().await,
        }
    }
}
