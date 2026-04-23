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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::datadog::test_support::{with_empty_home, EnvGuard};

    #[test]
    fn datadog_subcommands_auth_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Auth(_)));
    }

    #[tokio::test]
    async fn datadog_command_dispatches_auth_logout() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Logout(auth::LogoutCommand),
            }),
        };
        cmd.execute().await.unwrap();
    }
}
