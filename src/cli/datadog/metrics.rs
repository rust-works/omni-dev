//! CLI commands for Datadog metrics queries.

pub(crate) mod query;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Queries Datadog metrics.
#[derive(Parser)]
pub struct MetricsCommand {
    /// The metrics subcommand to execute.
    #[command(subcommand)]
    pub command: MetricsSubcommands,
}

/// Metrics subcommands.
#[derive(Subcommand)]
pub enum MetricsSubcommands {
    /// Executes a point-in-time metrics timeseries query.
    Query(query::QueryCommand),
}

impl MetricsCommand {
    /// Executes the metrics command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            MetricsSubcommands::Query(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::datadog::format::OutputFormat;

    #[test]
    fn metrics_subcommands_query_variant() {
        let cmd = MetricsCommand {
            command: MetricsSubcommands::Query(query::QueryCommand {
                query: "m".into(),
                from: "1h".into(),
                to: None,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, MetricsSubcommands::Query(_)));
    }
}
