//! CLI commands for Datadog metrics queries.

pub(crate) mod catalog;
pub(crate) mod query;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

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
    /// Executes a point-in-time metrics timeseries query (mirrors the `datadog_metrics_query` MCP tool).
    Query(query::QueryCommand),
    /// Inspects the metric catalog (`/api/v1/metrics`) (mirrors the `datadog_metrics_catalog_list` MCP tool).
    Catalog(catalog::CatalogCommand),
}

impl MetricsCommand {
    /// Executes the metrics command.
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        match self.command {
            MetricsSubcommands::Query(cmd) => cmd.execute(client).await,
            MetricsSubcommands::Catalog(cmd) => cmd.execute(client).await,
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

    #[test]
    fn metrics_subcommands_catalog_variant() {
        let cmd = MetricsCommand {
            command: MetricsSubcommands::Catalog(catalog::CatalogCommand {
                command: catalog::CatalogSubcommands::List(catalog::list::ListCommand {
                    host: None,
                    from: None,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, MetricsSubcommands::Catalog(_)));
    }
}
