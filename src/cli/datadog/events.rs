//! CLI commands for Datadog events stream.

pub(crate) mod list;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

/// Inspects the Datadog events stream.
#[derive(Parser)]
pub struct EventsCommand {
    /// The events subcommand to execute.
    #[command(subcommand)]
    pub command: EventsSubcommands,
}

/// Events subcommands.
#[derive(Subcommand)]
pub enum EventsSubcommands {
    /// Lists events via `GET /api/v2/events` (mirrors the `datadog_events_list` MCP tool).
    List(list::ListCommand),
}

impl EventsCommand {
    /// Executes the events command.
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        match self.command {
            EventsSubcommands::List(cmd) => cmd.execute(client).await,
        }
    }
}

/// One row of the bespoke `TIMESTAMP | TITLE | SOURCE | HOST | TAGS` event table.
pub(crate) struct EventRow<'a> {
    /// Event timestamp; `-` for unknown.
    pub timestamp: &'a str,
    /// Event title; `-` for unknown.
    pub title: &'a str,
    /// Source name; `-` for unknown.
    pub source: &'a str,
    /// Host name; `-` for unknown.
    pub host: &'a str,
    /// Tags applied to the event.
    pub tags: &'a [String],
}

/// Renders a list of [`EventRow`]s as an aligned text table.
pub(crate) fn render_event_table(rows: &[EventRow<'_>], out: &mut dyn Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "No events returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let tag_strings: Vec<String> = rows.iter().map(|r| r.tags.join(",")).collect();

    let ts_w = "TIMESTAMP"
        .len()
        .max(rows.iter().map(|r| r.timestamp.len()).max().unwrap_or(0));
    let title_w = "TITLE"
        .len()
        .max(rows.iter().map(|r| r.title.len()).max().unwrap_or(0));
    let source_w = "SOURCE"
        .len()
        .max(rows.iter().map(|r| r.source.len()).max().unwrap_or(0));
    let host_w = "HOST"
        .len()
        .max(rows.iter().map(|r| r.host.len()).max().unwrap_or(0));
    let tags_w = "TAGS"
        .len()
        .max(tag_strings.iter().map(String::len).max().unwrap_or(0));

    write_row(
        out,
        "TIMESTAMP",
        "TITLE",
        "SOURCE",
        "HOST",
        "TAGS",
        ts_w,
        title_w,
        source_w,
        host_w,
        tags_w,
    )?;
    write_row(
        out,
        &"-".repeat(ts_w),
        &"-".repeat(title_w),
        &"-".repeat(source_w),
        &"-".repeat(host_w),
        &"-".repeat(tags_w),
        ts_w,
        title_w,
        source_w,
        host_w,
        tags_w,
    )?;
    for (i, row) in rows.iter().enumerate() {
        write_row(
            out,
            row.timestamp,
            row.title,
            row.source,
            row.host,
            &tag_strings[i],
            ts_w,
            title_w,
            source_w,
            host_w,
            tags_w,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    ts: &str,
    title: &str,
    source: &str,
    host: &str,
    tags: &str,
    ts_w: usize,
    title_w: usize,
    source_w: usize,
    host_w: usize,
    tags_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{ts:<ts_w$}  {title:<title_w$}  {source:<source_w$}  {host:<host_w$}  {tags:<tags_w$}"
    )
    .context("Failed to write event row")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `Write` impl that succeeds for the first `n` line-terminated rows
    /// and then fails. Mirrors the Phase 1 helper in `cli/datadog/monitor.rs`.
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

    // ── render_event_table ─────────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows_aligned() {
        let tags1 = vec!["env:prod".to_string()];
        let tags2: Vec<String> = vec![];
        let rows = [
            EventRow {
                timestamp: "2026-04-22T10:00:00.000Z",
                title: "Deploy",
                source: "github",
                host: "web-01",
                tags: &tags1,
            },
            EventRow {
                timestamp: "2026-04-22T11:00:00.000Z",
                title: "Restart",
                source: "kubernetes",
                host: "-",
                tags: &tags2,
            },
        ];
        let mut buf = Vec::new();
        render_event_table(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("TIMESTAMP"));
        assert!(out.contains("TITLE"));
        assert!(out.contains("SOURCE"));
        assert!(out.contains("HOST"));
        assert!(out.contains("TAGS"));
        assert!(out.contains("Deploy"));
        assert!(out.contains("Restart"));
        assert!(out.contains("env:prod"));
        // Header + separator + 2 rows.
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_event_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No events returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [EventRow {
            timestamp: "t",
            title: "x",
            source: "s",
            host: "h",
            tags: &tags,
        }];
        let err = render_event_table(&rows, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [EventRow {
            timestamp: "t",
            title: "x",
            source: "s",
            host: "h",
            tags: &tags,
        }];
        let err = render_event_table(&rows, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [EventRow {
            timestamp: "t",
            title: "x",
            source: "s",
            host: "h",
            tags: &tags,
        }];
        let err = render_event_table(&rows, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_event_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }

    // ── EventsCommand dispatch ─────────────────────────────────────

    #[test]
    fn events_subcommands_list_variant() {
        use crate::cli::datadog::format::OutputFormat;
        let cmd = EventsCommand {
            command: EventsSubcommands::List(list::ListCommand {
                filter: None,
                from: "1h".into(),
                to: "now".into(),
                limit: 50,
                sources: None,
                tags: None,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, EventsSubcommands::List(_)));
    }
}
