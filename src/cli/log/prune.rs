//! `omni-dev log prune` — bound the local log's on-disk growth.
//!
//! Removes records by age (`--older-than`) and/or by size (`--max-size`),
//! rewriting `log.jsonl` in place via a same-directory temp file + atomic
//! rename (see [`request_log::prune`]). `--dry-run` reports what would go
//! without touching the file.

use anyhow::{bail, Context, Result};
use clap::Parser;

use super::query;
use crate::request_log::{self, PruneOptions};

/// Prunes old records from the local invocation + HTTP request log.
#[derive(Parser)]
pub struct PruneCommand {
    /// Remove records older than this relative window (e.g. `7d`, `24h`, `2w`).
    #[arg(long, value_name = "DUR")]
    older_than: Option<String>,
    /// Trim the oldest records until the file is at most this size (e.g. `10mb`,
    /// `512kb`, `1048576`); applied after `--older-than`.
    #[arg(long, value_name = "SIZE")]
    max_size: Option<String>,
    /// Report what would be removed without modifying the log.
    #[arg(long)]
    dry_run: bool,
}

impl PruneCommand {
    /// Executes the `omni-dev log prune` command.
    pub fn execute(self) -> Result<()> {
        if self.older_than.is_none() && self.max_size.is_none() {
            bail!("nothing to prune: pass --older-than <DUR> and/or --max-size <SIZE>");
        }
        let older_than = match self.older_than.as_deref() {
            Some(s) => Some(query::parse_since(s).context("invalid --older-than")?),
            None => None,
        };
        let max_size = match self.max_size.as_deref() {
            Some(s) => Some(request_log::parse_size(s).context("invalid --max-size")?),
            None => None,
        };

        let path = request_log::log_file_path().context("could not resolve the log file path")?;
        let outcome = request_log::prune(
            &path,
            &PruneOptions {
                older_than,
                max_size,
                dry_run: self.dry_run,
            },
        )?;

        let verb = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        println!(
            "{verb} {} record(s); kept {} ({} → {}).",
            outcome.removed,
            outcome.kept,
            human_bytes(outcome.bytes_before),
            human_bytes(outcome.bytes_after),
        );
        Ok(())
    }
}

/// Formats a byte count as a short human string (`0 B`, `12.3 KB`, `4.5 MB`).
fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = n as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: Wrapped,
    }

    #[derive(clap::Subcommand)]
    enum Wrapped {
        Prune(PruneCommand),
    }

    fn parse(args: &[&str]) -> PruneCommand {
        let mut full = vec!["omni-dev", "prune"];
        full.extend_from_slice(args);
        match Wrapper::try_parse_from(full).unwrap().cmd {
            Wrapped::Prune(cmd) => cmd,
        }
    }

    #[test]
    fn parses_flags() {
        let cmd = parse(&["--older-than", "7d", "--max-size", "10mb", "--dry-run"]);
        assert_eq!(cmd.older_than.as_deref(), Some("7d"));
        assert_eq!(cmd.max_size.as_deref(), Some("10mb"));
        assert!(cmd.dry_run);
    }

    #[test]
    fn requires_at_least_one_bound() {
        assert!(parse(&[]).execute().is_err());
    }

    #[test]
    fn human_bytes_formats_each_scale() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
    }
}
