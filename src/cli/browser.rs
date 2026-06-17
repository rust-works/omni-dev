//! Browser bridge CLI commands.

pub(crate) mod bridge;
pub(crate) mod harvest;
pub(crate) mod request;

use std::path::Path;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::browser::auth;

/// Resolves a thin-client session token for `request` / `harvest`.
///
/// Tries `--token-file` then `OMNI_BRIDGE_TOKEN` (via
/// [`auth::resolve_existing_token`]); if neither is set, falls back to the
/// daemon-written token file ([`crate::daemon::paths::token_path`]) when it
/// exists, so clients work transparently against a daemon-hosted bridge. The
/// original "no token" error is preserved when nothing resolves.
pub(crate) fn resolve_client_token(token_file: Option<&Path>) -> Result<String> {
    match auth::resolve_existing_token(token_file) {
        Ok(token) => Ok(token),
        Err(e) => {
            if token_file.is_none() {
                if let Ok(path) = crate::daemon::paths::token_path() {
                    if path.exists() {
                        return auth::resolve_existing_token(Some(&path));
                    }
                }
            }
            Err(e)
        }
    }
}

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
