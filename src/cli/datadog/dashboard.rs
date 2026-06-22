//! CLI commands for Datadog dashboard list / get.

pub(crate) mod get;
pub(crate) mod list;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

/// Inspects Datadog dashboards.
#[derive(Parser)]
pub struct DashboardCommand {
    /// The dashboard subcommand to execute.
    #[command(subcommand)]
    pub command: DashboardSubcommands,
}

/// Dashboard subcommands.
#[derive(Subcommand)]
pub enum DashboardSubcommands {
    /// Lists dashboards, optionally filtered to shared dashboards.
    List(list::ListCommand),
    /// Fetches a single dashboard definition by id.
    Get(get::GetCommand),
}

impl DashboardCommand {
    /// Executes the dashboard command.
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        match self.command {
            DashboardSubcommands::List(cmd) => cmd.execute(client).await,
            DashboardSubcommands::Get(cmd) => cmd.execute(client).await,
        }
    }
}

/// One row of the bespoke `ID | TITLE | AUTHOR | URL` dashboard table.
///
/// Rows borrow from the source [`DashboardSummary`] / [`Dashboard`]
/// rather than copying fields, so the renderer is allocation-free.
///
/// [`DashboardSummary`]: crate::datadog::types::DashboardSummary
/// [`Dashboard`]: crate::datadog::types::Dashboard
pub(crate) struct DashboardRow<'a> {
    /// Datadog dashboard identifier.
    pub id: &'a str,
    /// Human-readable title.
    pub title: &'a str,
    /// Author handle; `-` for unknown.
    pub author: &'a str,
    /// Web UI URL; `-` for unknown.
    pub url: &'a str,
}

/// Renders a list of [`DashboardRow`]s as an aligned text table.
///
/// Column layout: `ID | TITLE | AUTHOR | URL`. An empty input prints
/// `No dashboards returned.` so the user always sees something rather
/// than an empty buffer.
pub(crate) fn render_dashboard_table(rows: &[DashboardRow<'_>], out: &mut dyn Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "No dashboards returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let id_width = "ID"
        .len()
        .max(rows.iter().map(|r| r.id.len()).max().unwrap_or(0));
    let title_width = "TITLE"
        .len()
        .max(rows.iter().map(|r| r.title.len()).max().unwrap_or(0));
    let author_width = "AUTHOR"
        .len()
        .max(rows.iter().map(|r| r.author.len()).max().unwrap_or(0));
    let url_width = "URL"
        .len()
        .max(rows.iter().map(|r| r.url.len()).max().unwrap_or(0));

    write_row(
        out,
        "ID",
        "TITLE",
        "AUTHOR",
        "URL",
        id_width,
        title_width,
        author_width,
        url_width,
    )?;
    write_row(
        out,
        &"-".repeat(id_width),
        &"-".repeat(title_width),
        &"-".repeat(author_width),
        &"-".repeat(url_width),
        id_width,
        title_width,
        author_width,
        url_width,
    )?;
    for row in rows {
        write_row(
            out,
            row.id,
            row.title,
            row.author,
            row.url,
            id_width,
            title_width,
            author_width,
            url_width,
        )?;
    }
    Ok(())
}

/// Writes a single row of the bespoke dashboard table with consistent
/// 2-space gutters between cells.
#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    id: &str,
    title: &str,
    author: &str,
    url: &str,
    id_w: usize,
    title_w: usize,
    author_w: usize,
    url_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{id:<id_w$}  {title:<title_w$}  {author:<author_w$}  {url:<url_w$}"
    )
    .context("Failed to write dashboard row")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `Write` impl that succeeds for the first `n` line-terminated rows
    /// and then fails. Used to exercise each `?`-propagation site in
    /// [`render_dashboard_table`] independently.
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

    // ── render_dashboard_table ─────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows_aligned() {
        let rows = [
            DashboardRow {
                id: "abc",
                title: "Service A",
                author: "alice",
                url: "/dashboard/abc",
            },
            DashboardRow {
                id: "long-id-here",
                title: "Service B (longer title)",
                author: "-",
                url: "/dashboard/long-id-here",
            },
        ];
        let mut buf = Vec::new();
        render_dashboard_table(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("ID"));
        assert!(out.contains("TITLE"));
        assert!(out.contains("AUTHOR"));
        assert!(out.contains("URL"));

        assert!(out.contains("abc "));
        assert!(out.contains("long-id-here"));
        assert!(out.contains("Service A"));
        assert!(out.contains("Service B (longer title)"));
        assert!(out.contains("alice"));
        assert!(out.contains("/dashboard/abc"));

        // Header + separator + 2 data rows = 4 lines.
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_dashboard_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No dashboards returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let rows = [DashboardRow {
            id: "x",
            title: "y",
            author: "-",
            url: "-",
        }];
        let err = render_dashboard_table(&rows, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let rows = [DashboardRow {
            id: "x",
            title: "y",
            author: "-",
            url: "-",
        }];
        let err = render_dashboard_table(&rows, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let rows = [DashboardRow {
            id: "x",
            title: "y",
            author: "-",
            url: "-",
        }];
        let err = render_dashboard_table(&rows, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_dashboard_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }

    // ── DashboardCommand dispatch ──────────────────────────────────

    use crate::cli::datadog::format::OutputFormat;

    #[test]
    fn dashboard_subcommands_list_variant() {
        let cmd = DashboardCommand {
            command: DashboardSubcommands::List(list::ListCommand {
                filter_shared: false,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, DashboardSubcommands::List(_)));
    }

    #[test]
    fn dashboard_subcommands_get_variant() {
        let cmd = DashboardCommand {
            command: DashboardSubcommands::Get(get::GetCommand {
                id: "abc".into(),
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, DashboardSubcommands::Get(_)));
    }
}
