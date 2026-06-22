//! CLI commands for Datadog scheduled downtimes.

pub(crate) mod list;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

/// Inspects Datadog scheduled downtimes.
#[derive(Parser)]
pub struct DowntimeCommand {
    /// The downtime subcommand to execute.
    #[command(subcommand)]
    pub command: DowntimeSubcommands,
}

/// Downtime subcommands.
#[derive(Subcommand)]
pub enum DowntimeSubcommands {
    /// Lists scheduled downtimes via `GET /api/v1/downtime`.
    List(list::ListCommand),
}

impl DowntimeCommand {
    /// Executes the downtime command.
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        match self.command {
            DowntimeSubcommands::List(cmd) => cmd.execute(client).await,
        }
    }
}

/// One row of the bespoke `ID | SCOPE | START | END | MONITOR | MESSAGE` downtime table.
pub(crate) struct DowntimeRow<'a> {
    pub id: i64,
    pub scope: &'a str,
    pub start: &'a str,
    pub end: &'a str,
    pub monitor: &'a str,
    pub message: &'a str,
}

/// Renders a list of [`DowntimeRow`]s as an aligned text table.
pub(crate) fn render_downtime_table(rows: &[DowntimeRow<'_>], out: &mut dyn Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "No downtimes returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let id_strings: Vec<String> = rows.iter().map(|r| r.id.to_string()).collect();

    let id_w = "ID"
        .len()
        .max(id_strings.iter().map(String::len).max().unwrap_or(0));
    let scope_w = "SCOPE"
        .len()
        .max(rows.iter().map(|r| r.scope.len()).max().unwrap_or(0));
    let start_w = "START"
        .len()
        .max(rows.iter().map(|r| r.start.len()).max().unwrap_or(0));
    let end_w = "END"
        .len()
        .max(rows.iter().map(|r| r.end.len()).max().unwrap_or(0));
    let mon_w = "MONITOR"
        .len()
        .max(rows.iter().map(|r| r.monitor.len()).max().unwrap_or(0));
    let msg_w = "MESSAGE"
        .len()
        .max(rows.iter().map(|r| r.message.len()).max().unwrap_or(0));

    write_row(
        out, "ID", "SCOPE", "START", "END", "MONITOR", "MESSAGE", id_w, scope_w, start_w, end_w,
        mon_w, msg_w,
    )?;
    write_row(
        out,
        &"-".repeat(id_w),
        &"-".repeat(scope_w),
        &"-".repeat(start_w),
        &"-".repeat(end_w),
        &"-".repeat(mon_w),
        &"-".repeat(msg_w),
        id_w,
        scope_w,
        start_w,
        end_w,
        mon_w,
        msg_w,
    )?;
    for (i, row) in rows.iter().enumerate() {
        write_row(
            out,
            &id_strings[i],
            row.scope,
            row.start,
            row.end,
            row.monitor,
            row.message,
            id_w,
            scope_w,
            start_w,
            end_w,
            mon_w,
            msg_w,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    id: &str,
    scope: &str,
    start: &str,
    end: &str,
    monitor: &str,
    message: &str,
    id_w: usize,
    scope_w: usize,
    start_w: usize,
    end_w: usize,
    mon_w: usize,
    msg_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{id:<id_w$}  {scope:<scope_w$}  {start:<start_w$}  {end:<end_w$}  {monitor:<mon_w$}  {message:<msg_w$}"
    )
    .context("Failed to write downtime row")?;
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

    // ── render_downtime_table ──────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows_aligned() {
        let rows = [DowntimeRow {
            id: 1,
            scope: "env:prod",
            start: "2026-04-22T10:00:00Z",
            end: "2026-04-22T11:00:00Z",
            monitor: "12345",
            message: "Maintenance",
        }];
        let mut buf = Vec::new();
        render_downtime_table(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("SCOPE"));
        assert!(out.contains("START"));
        assert!(out.contains("END"));
        assert!(out.contains("MONITOR"));
        assert!(out.contains("MESSAGE"));
        assert!(out.contains("12345"));
        assert!(out.contains("Maintenance"));
        assert_eq!(out.lines().count(), 3); // Header + separator + 1 row.
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_downtime_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No downtimes returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let rows = [DowntimeRow {
            id: 1,
            scope: "*",
            start: "-",
            end: "-",
            monitor: "-",
            message: "-",
        }];
        let err = render_downtime_table(&rows, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let rows = [DowntimeRow {
            id: 1,
            scope: "*",
            start: "-",
            end: "-",
            monitor: "-",
            message: "-",
        }];
        let err = render_downtime_table(&rows, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let rows = [DowntimeRow {
            id: 1,
            scope: "*",
            start: "-",
            end: "-",
            monitor: "-",
            message: "-",
        }];
        let err = render_downtime_table(&rows, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_downtime_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }
}
