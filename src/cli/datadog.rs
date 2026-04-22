//! Datadog CLI commands (read-only).

pub(crate) mod auth;
pub(crate) mod format;
pub(crate) mod helpers;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Datadog: read-only API operations.
#[derive(Parser)]
pub struct DatadogCommand {
    /// The Datadog subcommand to execute.
    #[command(subcommand)]
    pub command: DatadogSubcommands,
}

/// Datadog subcommands.
#[derive(Subcommand)]
pub enum DatadogSubcommands {
    /// Manages Datadog API credentials.
    Auth(auth::AuthCommand),
}

impl DatadogCommand {
    /// Executes the Datadog command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            DatadogSubcommands::Auth(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datadog_subcommands_auth_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Auth(_)));
    }
}
