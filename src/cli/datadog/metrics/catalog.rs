//! CLI commands for the Datadog metric catalog (`/api/v1/metrics`).
//!
//! Distinct from `metrics query` (`/api/v1/query`): the catalog returns
//! the *names* of metrics ingested since `from`; `query` returns
//! timeseries data for a specific metric.

pub(crate) mod list;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Inspects the Datadog metric catalog.
#[derive(Parser)]
pub struct CatalogCommand {
    /// The catalog subcommand to execute.
    #[command(subcommand)]
    pub command: CatalogSubcommands,
}

/// Catalog subcommands.
#[derive(Subcommand)]
pub enum CatalogSubcommands {
    /// Lists metrics in the catalog.
    List(list::ListCommand),
}

impl CatalogCommand {
    /// Executes the catalog command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            CatalogSubcommands::List(cmd) => cmd.execute().await,
        }
    }
}

/// Renders the metric catalog as a single-column text table.
pub(crate) fn render_metrics_table(metrics: &[String], out: &mut dyn Write) -> Result<()> {
    if metrics.is_empty() {
        writeln!(out, "No metrics returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }
    let width = "METRIC"
        .len()
        .max(metrics.iter().map(String::len).max().unwrap_or(0));
    writeln!(out, "{header:<width$}", header = "METRIC")
        .context("Failed to write metric header")?;
    writeln!(out, "{}", "-".repeat(width)).context("Failed to write metric separator")?;
    for m in metrics {
        writeln!(out, "{m:<width$}").context("Failed to write metric row")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    struct FailAfter {
        successes_remaining: usize,
        sink: Vec<u8>,
    }

    impl FailAfter {
        fn new(n: usize) -> Self {
            Self {
                successes_remaining: n,
                sink: Vec::new(),
            }
        }
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.successes_remaining == 0 {
                return Err(std::io::Error::other("test forced write failure"));
            }
            self.sink.extend_from_slice(buf);
            if buf.contains(&b'\n') {
                self.successes_remaining -= 1;
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // ── render_metrics_table ───────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows() {
        let metrics = vec!["system.cpu.user".to_string(), "system.cpu.idle".to_string()];
        let mut buf = Vec::new();
        render_metrics_table(&metrics, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("METRIC"));
        assert!(out.contains("system.cpu.user"));
        assert!(out.contains("system.cpu.idle"));
        // Header + separator + 2 rows.
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_metrics_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No metrics returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let metrics = vec!["m".to_string()];
        let err = render_metrics_table(&metrics, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let metrics = vec!["m".to_string()];
        let err = render_metrics_table(&metrics, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let metrics = vec!["m".to_string()];
        let err = render_metrics_table(&metrics, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_metrics_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }
}
