//! `omni-dev log` — search and pretty-print the local invocation + HTTP log.
//!
//! Read-only and synchronous. Streams [`request_log::log_file_path`] line by
//! line, applies the filter matrix, and renders each match as `oneline`,
//! `json` (byte-identical to the on-disk NDJSON), or `full`.

mod format;
mod query;
mod stream;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::request_log;
use query::Filter;

/// Output rendering for `omni-dev log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Format {
    /// One compact line per record (default).
    Oneline,
    /// The on-disk NDJSON line, verbatim (composes with `jq`).
    Json,
    /// A labelled, multi-line block per record.
    Full,
}

/// Searches and pretty-prints the local invocation + HTTP request log.
#[derive(Parser)]
pub struct LogCommand {
    /// Only records newer than this relative window (e.g. `30m`, `2h`, `1d`).
    #[arg(long, value_name = "DUR")]
    since: Option<String>,
    /// Match the HTTP method (case-insensitive), e.g. `GET`.
    #[arg(long, value_name = "METHOD")]
    method: Option<String>,
    /// Match the status: exact (`200`), class (`5xx`), or list (`4xx,5xx`).
    #[arg(long, value_name = "STATUS")]
    status: Option<String>,
    /// Match the service tag, e.g. `jira`, `datadog`, `browser-bridge`.
    #[arg(long, value_name = "NAME")]
    service: Option<String>,
    /// Match the resolved command path prefix, e.g. `"jira read"`.
    #[arg(long, value_name = "PATH")]
    command: Option<String>,
    /// Match a substring of the request URL.
    #[arg(long, value_name = "SUBSTR")]
    url: Option<String>,
    /// Match a regular expression against the raw JSON line.
    #[arg(long, value_name = "REGEX")]
    grep: Option<String>,
    /// Require a fuzzy token (substring of the raw line); repeatable, AND-ed.
    #[arg(long, value_name = "TOKEN")]
    fuzzy: Vec<String>,
    /// A query expression (AND/OR/NOT, `field:value`, bare tokens); repeatable,
    /// AND-ed together.
    #[arg(long, value_name = "EXPR")]
    query: Vec<String>,
    /// Match this record `id` or `invocation_id` (pulls a run and its requests).
    #[arg(long, value_name = "ID")]
    id: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Oneline)]
    format: Format,
    /// Show at most N (most recent) matching records.
    #[arg(short = 'n', long, value_name = "N")]
    limit: Option<usize>,
    /// Follow the log, printing new matching records as they are appended.
    #[arg(short = 'f', long)]
    follow: bool,
}

impl LogCommand {
    /// Executes the `omni-dev log` command.
    pub fn execute(self) -> Result<()> {
        let path = request_log::log_file_path().context("could not resolve the log file path")?;
        let filter = Filter::build(query::FilterInput {
            since: self.since.as_deref(),
            method: self.method.as_deref(),
            status: self.status.as_deref(),
            service: self.service.as_deref(),
            command: self.command.as_deref(),
            url: self.url.as_deref(),
            grep: self.grep.as_deref(),
            fuzzy: &self.fuzzy,
            query: &self.query,
            id: self.id.as_deref(),
        })?;
        stream::run(&path, &filter, self.format, self.limit, self.follow)
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
        Log(LogCommand),
    }

    fn parse(args: &[&str]) -> LogCommand {
        let mut full = vec!["omni-dev", "log"];
        full.extend_from_slice(args);
        match Wrapper::try_parse_from(full).unwrap().cmd {
            Wrapped::Log(cmd) => cmd,
        }
    }

    #[test]
    fn defaults_are_sane() {
        let cmd = parse(&[]);
        assert_eq!(cmd.format, Format::Oneline);
        assert!(cmd.limit.is_none());
        assert!(!cmd.follow);
    }

    #[test]
    fn parses_full_flag_matrix() {
        let cmd = parse(&[
            "--since",
            "2h",
            "--method",
            "GET",
            "--status",
            "5xx",
            "--service",
            "jira",
            "--command",
            "jira read",
            "--url",
            "issue",
            "--grep",
            "X-\\d+",
            "--fuzzy",
            "a",
            "--fuzzy",
            "b",
            "--query",
            "status:5xx OR method:POST",
            "--id",
            "abc",
            "--format",
            "json",
            "-n",
            "10",
            "-f",
        ]);
        assert_eq!(cmd.since.as_deref(), Some("2h"));
        assert_eq!(cmd.status.as_deref(), Some("5xx"));
        assert_eq!(cmd.fuzzy, vec!["a", "b"]);
        assert_eq!(cmd.query.len(), 1);
        assert_eq!(cmd.format, Format::Json);
        assert_eq!(cmd.limit, Some(10));
        assert!(cmd.follow);
    }
}
