//! Datadog CLI commands (read-only).

pub(crate) mod auth;
pub(crate) mod format;
pub(crate) mod helpers;
pub(crate) mod metrics;

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
    /// Queries Datadog metrics.
    Metrics(metrics::MetricsCommand),
}

impl DatadogCommand {
    /// Executes the Datadog command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            DatadogSubcommands::Auth(cmd) => cmd.execute().await,
            DatadogSubcommands::Metrics(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::datadog::format::OutputFormat;
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

    #[test]
    fn datadog_subcommands_metrics_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Metrics(metrics::MetricsCommand {
                command: metrics::MetricsSubcommands::Query(metrics::query::QueryCommand {
                    query: "m".into(),
                    from: "1h".into(),
                    to: None,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Metrics(_)));
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

    #[tokio::test]
    async fn datadog_command_dispatches_metrics_query() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Metrics(metrics::MetricsCommand {
                command: metrics::MetricsSubcommands::Query(metrics::query::QueryCommand {
                    query: "m".into(),
                    from: "1h".into(),
                    to: None,
                    output: OutputFormat::Table,
                }),
            }),
        };
        // Fails at credential loading, not at dispatch — which is what we're
        // verifying here: the Metrics arm is wired through.
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }
}
