//! CLI commands for Datadog Service Level Objectives (list / get).

pub(crate) mod get;
pub(crate) mod list;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

/// Inspects Datadog Service Level Objectives.
#[derive(Parser)]
pub struct SloCommand {
    /// The SLO subcommand to execute.
    #[command(subcommand)]
    pub command: SloSubcommands,
}

/// SLO subcommands.
#[derive(Subcommand)]
pub enum SloSubcommands {
    /// Lists SLOs with optional tag filter (mirrors the `datadog_slo_list` MCP tool).
    List(list::ListCommand),
    /// Fetches a single SLO definition by id (mirrors the `datadog_slo_get` MCP tool).
    Get(get::GetCommand),
}

impl SloCommand {
    /// Executes the SLO command.
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        match self.command {
            SloSubcommands::List(cmd) => cmd.execute(client).await,
            SloSubcommands::Get(cmd) => cmd.execute(client).await,
        }
    }
}

/// One row of the bespoke `ID | NAME | TYPE | TAGS` SLO table.
pub(crate) struct SloRow<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub slo_type: &'a str,
    pub tags: &'a [String],
}

/// Renders a list of [`SloRow`]s as an aligned text table.
pub(crate) fn render_slo_table(rows: &[SloRow<'_>], out: &mut dyn Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "No SLOs returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let tag_strings: Vec<String> = rows.iter().map(|r| r.tags.join(",")).collect();

    let id_w = "ID"
        .len()
        .max(rows.iter().map(|r| r.id.len()).max().unwrap_or(0));
    let name_w = "NAME"
        .len()
        .max(rows.iter().map(|r| r.name.len()).max().unwrap_or(0));
    let type_w = "TYPE"
        .len()
        .max(rows.iter().map(|r| r.slo_type.len()).max().unwrap_or(0));
    let tags_w = "TAGS"
        .len()
        .max(tag_strings.iter().map(String::len).max().unwrap_or(0));

    write_row(
        out, "ID", "NAME", "TYPE", "TAGS", id_w, name_w, type_w, tags_w,
    )?;
    write_row(
        out,
        &"-".repeat(id_w),
        &"-".repeat(name_w),
        &"-".repeat(type_w),
        &"-".repeat(tags_w),
        id_w,
        name_w,
        type_w,
        tags_w,
    )?;
    for (i, row) in rows.iter().enumerate() {
        write_row(
            out,
            row.id,
            row.name,
            row.slo_type,
            &tag_strings[i],
            id_w,
            name_w,
            type_w,
            tags_w,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    id: &str,
    name: &str,
    slo_type: &str,
    tags: &str,
    id_w: usize,
    name_w: usize,
    type_w: usize,
    tags_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{id:<id_w$}  {name:<name_w$}  {slo_type:<type_w$}  {tags:<tags_w$}"
    )
    .context("Failed to write SLO row")?;
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

    // ── render_slo_table ───────────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows_aligned() {
        let tags1 = vec!["team:sre".to_string()];
        let tags2: Vec<String> = vec![];
        let rows = [
            SloRow {
                id: "abc",
                name: "Latency",
                slo_type: "metric",
                tags: &tags1,
            },
            SloRow {
                id: "def-ghi-jkl",
                name: "Uptime",
                slo_type: "monitor",
                tags: &tags2,
            },
        ];
        let mut buf = Vec::new();
        render_slo_table(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("NAME"));
        assert!(out.contains("TYPE"));
        assert!(out.contains("TAGS"));
        assert!(out.contains("Latency"));
        assert!(out.contains("Uptime"));
        assert!(out.contains("team:sre"));
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_slo_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No SLOs returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [SloRow {
            id: "x",
            name: "y",
            slo_type: "metric",
            tags: &tags,
        }];
        let err = render_slo_table(&rows, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [SloRow {
            id: "x",
            name: "y",
            slo_type: "metric",
            tags: &tags,
        }];
        let err = render_slo_table(&rows, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let tags: Vec<String> = vec![];
        let rows = [SloRow {
            id: "x",
            name: "y",
            slo_type: "metric",
            tags: &tags,
        }];
        let err = render_slo_table(&rows, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_slo_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }
}
