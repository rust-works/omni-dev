//! CLI commands for Datadog hosts.

pub(crate) mod list;

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Inspects Datadog reporting hosts.
#[derive(Parser)]
pub struct HostsCommand {
    /// The hosts subcommand to execute.
    #[command(subcommand)]
    pub command: HostsSubcommands,
}

/// Hosts subcommands.
#[derive(Subcommand)]
pub enum HostsSubcommands {
    /// Lists hosts via `GET /api/v1/hosts`.
    List(list::ListCommand),
}

impl HostsCommand {
    /// Executes the hosts command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            HostsSubcommands::List(cmd) => cmd.execute().await,
        }
    }
}

/// One row of the bespoke `NAME | UP | LAST REPORTED | APPS` host table.
pub(crate) struct HostRow<'a> {
    pub name: &'a str,
    pub up: &'a str,
    pub last_reported: &'a str,
    pub apps: &'a [String],
}

/// Renders a list of [`HostRow`]s as an aligned text table.
pub(crate) fn render_host_table(rows: &[HostRow<'_>], out: &mut dyn Write) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "No hosts returned.").context("Failed to write empty-table message")?;
        return Ok(());
    }

    let app_strings: Vec<String> = rows.iter().map(|r| r.apps.join(",")).collect();

    let name_w = "NAME"
        .len()
        .max(rows.iter().map(|r| r.name.len()).max().unwrap_or(0));
    let up_w = "UP"
        .len()
        .max(rows.iter().map(|r| r.up.len()).max().unwrap_or(0));
    let lr_w = "LAST REPORTED".len().max(
        rows.iter()
            .map(|r| r.last_reported.len())
            .max()
            .unwrap_or(0),
    );
    let apps_w = "APPS"
        .len()
        .max(app_strings.iter().map(String::len).max().unwrap_or(0));

    write_row(
        out,
        "NAME",
        "UP",
        "LAST REPORTED",
        "APPS",
        name_w,
        up_w,
        lr_w,
        apps_w,
    )?;
    write_row(
        out,
        &"-".repeat(name_w),
        &"-".repeat(up_w),
        &"-".repeat(lr_w),
        &"-".repeat(apps_w),
        name_w,
        up_w,
        lr_w,
        apps_w,
    )?;
    for (i, row) in rows.iter().enumerate() {
        write_row(
            out,
            row.name,
            row.up,
            row.last_reported,
            &app_strings[i],
            name_w,
            up_w,
            lr_w,
            apps_w,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_row(
    out: &mut dyn Write,
    name: &str,
    up: &str,
    lr: &str,
    apps: &str,
    name_w: usize,
    up_w: usize,
    lr_w: usize,
    apps_w: usize,
) -> Result<()> {
    writeln!(
        out,
        "{name:<name_w$}  {up:<up_w$}  {lr:<lr_w$}  {apps:<apps_w$}"
    )
    .context("Failed to write host row")?;
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

    // ── render_host_table ──────────────────────────────────────────

    #[test]
    fn render_table_writes_header_and_rows_aligned() {
        let apps1 = vec!["nginx".to_string(), "ntp".to_string()];
        let apps2: Vec<String> = vec![];
        let rows = [
            HostRow {
                name: "web-01",
                up: "yes",
                last_reported: "2026-04-22T10:00:00Z",
                apps: &apps1,
            },
            HostRow {
                name: "web-02",
                up: "no",
                last_reported: "-",
                apps: &apps2,
            },
        ];
        let mut buf = Vec::new();
        render_host_table(&rows, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("NAME"));
        assert!(out.contains("UP"));
        assert!(out.contains("LAST REPORTED"));
        assert!(out.contains("APPS"));
        assert!(out.contains("web-01"));
        assert!(out.contains("web-02"));
        assert!(out.contains("nginx,ntp"));
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn render_table_empty_prints_message() {
        let mut buf = Vec::new();
        render_host_table(&[], &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No hosts returned.\n");
    }

    #[test]
    fn render_table_propagates_header_write_errors() {
        let apps: Vec<String> = vec![];
        let rows = [HostRow {
            name: "h",
            up: "yes",
            last_reported: "-",
            apps: &apps,
        }];
        let err = render_host_table(&rows, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_separator_write_errors() {
        let apps: Vec<String> = vec![];
        let rows = [HostRow {
            name: "h",
            up: "yes",
            last_reported: "-",
            apps: &apps,
        }];
        let err = render_host_table(&rows, &mut FailAfter::new(1)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_propagates_data_row_write_errors() {
        let apps: Vec<String> = vec![];
        let rows = [HostRow {
            name: "h",
            up: "yes",
            last_reported: "-",
            apps: &apps,
        }];
        let err = render_host_table(&rows, &mut FailAfter::new(2)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_table_empty_propagates_write_errors() {
        let err = render_host_table(&[], &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("empty-table message"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }
}
