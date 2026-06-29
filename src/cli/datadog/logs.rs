//! CLI commands for Datadog logs search.

pub(crate) mod search;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

/// Searches Datadog logs.
#[derive(Parser)]
pub struct LogsCommand {
    /// The logs subcommand to execute.
    #[command(subcommand)]
    pub command: LogsSubcommands,
}

/// Logs subcommands.
#[derive(Subcommand)]
pub enum LogsSubcommands {
    /// Searches log events via `POST /api/v2/logs/events/search` (mirrors the `datadog_logs_search` MCP tool).
    Search(search::SearchCommand),
}

impl LogsCommand {
    /// Executes the logs command.
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        match self.command {
            LogsSubcommands::Search(cmd) => cmd.execute(client).await,
        }
    }
}

/// One row of the bespoke `TIMESTAMP | SERVICE | STATUS | MESSAGE` log table.
///
/// Rows borrow from the source [`LogEvent`] rather than copying fields,
/// so the renderer is allocation-free.
///
/// [`LogEvent`]: crate::datadog::types::LogEvent
pub(crate) struct LogRow<'a> {
    /// Event timestamp; `-` for unknown.
    pub timestamp: &'a str,
    /// Service name; `-` for unknown.
    pub service: &'a str,
    /// Log status (e.g. `info`, `error`); `-` for unknown.
    pub status: &'a str,
    /// Free-form message; empty string for unknown.
    pub message: &'a str,
}

/// Renders a list of [`LogRow`]s as an aligned text table.
///
/// Column layout: `TIMESTAMP | SERVICE | STATUS | MESSAGE`. An empty
/// input prints `No logs returned.` so the user always sees something
/// rather than an empty buffer.
pub(crate) fn render_log_table(rows: &[LogRow<'_>], out: &mut dyn Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "No logs returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let ts_width = "TIMESTAMP"
        .len()
        .max(rows.iter().map(|r| r.timestamp.len()).max().unwrap_or(0));
    let service_width = "SERVICE"
        .len()
        .max(rows.iter().map(|r| r.service.len()).max().unwrap_or(0));
    let status_width = "STATUS"
        .len()
        .max(rows.iter().map(|r| r.status.len()).max().unwrap_or(0));
    let message_width = "MESSAGE"
        .len()
        .max(rows.iter().map(|r| r.message.len()).max().unwrap_or(0));

    write_row(
        out,
        "TIMESTAMP",
        "SERVICE",
        "STATUS",
        "MESSAGE",
        ts_width,
        service_width,
        status_width,
        message_width,
    )?;
    write_row(
        out,
        &"-".repeat(ts_width),
        &"-".repeat(service_width),
        &"-".repeat(status_width),
        &"-".repeat(message_width),
        ts_width,
        service_width,
        status_width,
        message_width,
    )?;
    for row in rows {
        write_row(
            out,
            row.timestamp,
            row.service,
            row.status,
            row.message,
            ts_width,
            service_width,
            status_width,
            message_width,
        )?;
    }
    Ok(())
}

/// Writes a single row of the bespoke log table with consistent
/// 2-space gutters between cells.
#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    timestamp: &str,
    service: &str,
    status: &str,
    message: &str,
    ts_w: usize,
    service_w: usize,
    status_w: usize,
    message_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{timestamp:<ts_w$}  {service:<service_w$}  {status:<status_w$}  {message:<message_w$}"
    )
    .context("Failed to write log row")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::datadog::format::OutputFormat;
    use crate::datadog::types::SortOrder;

    /// `Write` impl that succeeds for the first `n` line-terminated rows
    /// then fails. Used to exercise each `?`-propagation site in
    /// [`render_log_table`] independently.
    struct FailAfter {
        successes_remaining: usize,
        sink: Vec<u8>,
    }

    impl FailAfter {
        fn new(successes_remaining: usize) -> Self {
            Self {
                successes_remaining,
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

    // ── render_log_table ───────────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows_aligned() {
        let rows = [
            LogRow {
                timestamp: "2026-04-22T10:00:00.000Z",
                service: "api",
                status: "info",
                message: "hello",
            },
            LogRow {
                timestamp: "2026-04-22T10:00:01.000Z",
                service: "worker",
                status: "error",
                message: "boom",
            },
        ];
        let mut buf = Vec::new();
        render_log_table(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("TIMESTAMP"));
        assert!(out.contains("SERVICE"));
        assert!(out.contains("STATUS"));
        assert!(out.contains("MESSAGE"));

        assert!(out.contains("2026-04-22T10:00:00.000Z"));
        assert!(out.contains("api"));
        assert!(out.contains("worker"));
        assert!(out.contains("error"));
        assert!(out.contains("boom"));

        // Header + separator + 2 data rows = 4 lines.
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_log_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No logs returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let rows = [LogRow {
            timestamp: "t",
            service: "s",
            status: "i",
            message: "m",
        }];
        let err = render_log_table(&rows, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let rows = [LogRow {
            timestamp: "t",
            service: "s",
            status: "i",
            message: "m",
        }];
        let err = render_log_table(&rows, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let rows = [LogRow {
            timestamp: "t",
            service: "s",
            status: "i",
            message: "m",
        }];
        let err = render_log_table(&rows, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_log_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }

    // ── LogsCommand dispatch ───────────────────────────────────────

    #[test]
    fn logs_subcommands_search_variant() {
        let cmd = LogsCommand {
            command: LogsSubcommands::Search(search::SearchCommand {
                filter: "*".into(),
                from: "15m".into(),
                to: "now".into(),
                limit: 100,
                sort: search::SortArg::TimestampDesc,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, LogsSubcommands::Search(_)));
    }

    #[test]
    fn sort_arg_maps_to_sort_order() {
        assert_eq!(
            search::SortArg::TimestampAsc.to_sort_order(),
            SortOrder::TimestampAsc
        );
        assert_eq!(
            search::SortArg::TimestampDesc.to_sort_order(),
            SortOrder::TimestampDesc
        );
    }
}
