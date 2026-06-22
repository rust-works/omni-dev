//! CLI commands for Datadog monitor list / get / search.

pub(crate) mod get;
pub(crate) mod list;
pub(crate) mod search;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

/// Inspects Datadog monitors.
#[derive(Parser)]
pub struct MonitorCommand {
    /// The monitor subcommand to execute.
    #[command(subcommand)]
    pub command: MonitorSubcommands,
}

/// Monitor subcommands.
#[derive(Subcommand)]
pub enum MonitorSubcommands {
    /// Lists monitors with optional name / tag filters.
    List(list::ListCommand),
    /// Fetches a single monitor by id.
    Get(get::GetCommand),
    /// Searches monitors by free-text / faceted query.
    Search(search::SearchCommand),
}

impl MonitorCommand {
    /// Executes the monitor command.
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        match self.command {
            MonitorSubcommands::List(cmd) => cmd.execute(client).await,
            MonitorSubcommands::Get(cmd) => cmd.execute(client).await,
            MonitorSubcommands::Search(cmd) => cmd.execute(client).await,
        }
    }
}

/// One row of the bespoke `ID | NAME | STATUS | TAGS` monitor table.
///
/// Rows borrow from the source [`Monitor`] / [`MonitorSearchItem`]
/// rather than copying fields, so the renderer remains zero-allocation
/// for fixed-size cells (id is formatted into a small `String`, but
/// `name`, `status`, and `tags` are direct borrows).
///
/// [`Monitor`]: crate::datadog::types::Monitor
/// [`MonitorSearchItem`]: crate::datadog::types::MonitorSearchItem
pub(crate) struct MonitorRow<'a> {
    /// Datadog monitor identifier.
    pub id: i64,
    /// Human-readable monitor name.
    pub name: &'a str,
    /// Status string (e.g. `OK`, `Alert`, `ALERT`); `-` for unknown.
    pub status: &'a str,
    /// Tags applied to the monitor.
    pub tags: &'a [String],
}

/// Renders a list of [`MonitorRow`]s as an aligned text table.
///
/// Column layout: `ID | NAME | STATUS | TAGS`. Tags are joined with
/// commas. An empty input prints `No monitors returned.` so the user
/// always sees something rather than an empty buffer.
pub(crate) fn render_monitor_table(rows: &[MonitorRow<'_>], out: &mut dyn Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "No monitors returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let id_strings: Vec<String> = rows.iter().map(|r| r.id.to_string()).collect();
    let tag_strings: Vec<String> = rows.iter().map(|r| r.tags.join(",")).collect();

    let id_width = "ID"
        .len()
        .max(id_strings.iter().map(String::len).max().unwrap_or(0));
    let name_width = "NAME"
        .len()
        .max(rows.iter().map(|r| r.name.len()).max().unwrap_or(0));
    let status_width = "STATUS"
        .len()
        .max(rows.iter().map(|r| r.status.len()).max().unwrap_or(0));
    let tags_width = "TAGS"
        .len()
        .max(tag_strings.iter().map(String::len).max().unwrap_or(0));

    write_row(
        out,
        "ID",
        "NAME",
        "STATUS",
        "TAGS",
        id_width,
        name_width,
        status_width,
        tags_width,
    )?;
    write_row(
        out,
        &"-".repeat(id_width),
        &"-".repeat(name_width),
        &"-".repeat(status_width),
        &"-".repeat(tags_width),
        id_width,
        name_width,
        status_width,
        tags_width,
    )?;
    for (i, row) in rows.iter().enumerate() {
        write_row(
            out,
            &id_strings[i],
            row.name,
            row.status,
            &tag_strings[i],
            id_width,
            name_width,
            status_width,
            tags_width,
        )?;
    }
    Ok(())
}

/// Writes a single row of the bespoke monitor table with consistent
/// 2-space gutters between cells.
#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    id: &str,
    name: &str,
    status: &str,
    tags: &str,
    id_w: usize,
    name_w: usize,
    status_w: usize,
    tags_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{id:<id_w$}  {name:<name_w$}  {status:<status_w$}  {tags:<tags_w$}"
    )
    .context("Failed to write monitor row")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `Write` impl that succeeds for the first `n` writeln-bearing rows,
    /// then fails. Used to exercise each `?`-propagation site in
    /// [`render_monitor_table`] independently.
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
            // `writeln!` issues one `write` for the row body and one for `\n`;
            // counting both as a "row" is too coarse, so we treat any write
            // ending in `\n` as the end of a logical row.
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

    // ── render_monitor_table ───────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows_aligned() {
        let tags1 = vec!["env:prod".to_string(), "team:sre".to_string()];
        let tags2: Vec<String> = vec![];
        let rows = [
            MonitorRow {
                id: 1,
                name: "Disk full",
                status: "OK",
                tags: &tags1,
            },
            MonitorRow {
                id: 12345,
                name: "Latency high (p99)",
                status: "Alert",
                tags: &tags2,
            },
        ];
        let mut buf = Vec::new();
        render_monitor_table(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();

        // Header present
        assert!(out.contains("ID"));
        assert!(out.contains("NAME"));
        assert!(out.contains("STATUS"));
        assert!(out.contains("TAGS"));

        // Both rows present
        assert!(out.contains("1 "));
        assert!(out.contains("12345"));
        assert!(out.contains("Disk full"));
        assert!(out.contains("Latency high (p99)"));
        assert!(out.contains("Alert"));
        assert!(out.contains("env:prod,team:sre"));

        // Header + separator + 2 data rows = 4 lines.
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_monitor_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No monitors returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [MonitorRow {
            id: 1,
            name: "n",
            status: "OK",
            tags: &tags,
        }];
        // Fail on the very first write — the header row.
        let err = render_monitor_table(&rows, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [MonitorRow {
            id: 1,
            name: "n",
            status: "OK",
            tags: &tags,
        }];
        // Header succeeds, separator fails — exercises the second `?` site.
        let err = render_monitor_table(&rows, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [MonitorRow {
            id: 1,
            name: "n",
            status: "OK",
            tags: &tags,
        }];
        // Header + separator succeed, data row fails — exercises the
        // in-loop `?` site.
        let err = render_monitor_table(&rows, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_monitor_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        // The Write trait's `flush` impl is required by the trait but never
        // exercised through `render_monitor_table`. Cover it directly so
        // coverage tools don't flag it as unreachable.
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }
}
