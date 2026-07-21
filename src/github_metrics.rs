//! Counting every GitHub API invocation omni-dev makes (#1387).
//!
//! Every GitHub call funnels through the `gh` CLI subprocess (ADR-0003 /
//! ADR-0050 — the token never enters our process), so there is no in-process
//! HTTP transport to instrument. Instead, [`run_gh`] is the single choke point
//! every Rust `gh` call site routes through: it spawns `gh`, records one
//! `kind: "gh"` line to the request log ([`crate::request_log::record_gh`]), and
//! returns the process `Output` **unchanged** so call-site behavior and exit
//! codes are untouched.
//!
//! [`aggregate`] reads those records back and tallies them by category,
//! subcommand, and source. It is the shared backend for `omni-dev log count
//! --kind gh`, the daemon's periodic/shutdown summaries, and `daemon status`.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output};
use std::time::Instant;

use chrono::{DateTime, Utc};

use crate::request_log::{self, GhOutcome, LogRecord, RecordKind, Source};

/// Whether a `gh` invocation hit the GitHub API, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// A direct `gh api …` call (`api graphql`, `api rate_limit`).
    Api,
    /// A higher-level subcommand that hits the API indirectly (`pr list`,
    /// `pr create`, `repo view`, …).
    Subcommand,
    /// A local-only `gh` call that hits no API (`gh --version`). Excluded from
    /// the API-invocation total.
    Local,
}

impl Category {
    /// Derives the category from a record's `command` tokens (the split label).
    #[must_use]
    pub fn from_command(command: &[String]) -> Self {
        match command.first().map(String::as_str) {
            Some("api") => Self::Api,
            // A leading `--flag` (only `--version` today) is a local probe.
            Some(first) if first.starts_with('-') => Self::Local,
            _ => Self::Subcommand,
        }
    }

    /// Stable lowercase name, used for JSON map keys and display.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Subcommand => "subcommand",
            Self::Local => "local",
        }
    }
}

/// Lowercase name of a [`Source`] for display / JSON map keys.
#[must_use]
pub fn source_str(source: Source) -> &'static str {
    match source {
        Source::Cli => "cli",
        Source::Mcp => "mcp",
        Source::Daemon => "daemon",
        Source::Unknown => "unknown",
    }
}

/// Runs one `gh` invocation through the choke point, recording a `kind: "gh"`
/// request-log line for it.
///
/// `label` is the semantic subcommand (`"api graphql"`, `"pr list"`,
/// `"--version"`); it is split into the record's `command` for per-subcommand
/// aggregation and category derivation. `cwd`, when given, sets the child's
/// working directory (several call sites run `gh` inside a repo).
///
/// **Blocking** (`Command::output`) — daemon callers must already be on a
/// blocking thread. The `Output` (or spawn `io::Error`) is returned verbatim, so
/// this is a drop-in for an existing `Command::new("gh").…​.output()`; logging is
/// best-effort and never alters the result. Source is read from the ambient
/// [`request_log::current_context`], so no call site threads it.
pub fn run_gh<I, S>(bin: &Path, args: I, label: &str, cwd: Option<&Path>) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let argv: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();

    let mut cmd = Command::new(bin);
    cmd.args(&argv);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let started = Instant::now();
    let result = cmd.output();
    let duration = started.elapsed();

    let (exit_code, error) = match &result {
        Ok(output) => (output.status.code(), None),
        Err(e) => (None, Some(e.to_string())),
    };
    request_log::record_gh(GhOutcome {
        label: label.to_string(),
        argv,
        exit_code,
        duration,
        error,
    });

    result
}

/// Tallies of `gh` invocations over a time window, broken down three ways.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhCounts {
    /// Count per category (api / subcommand / local).
    pub by_category: BTreeMap<Category, u64>,
    /// Count per semantic subcommand (`"api graphql"`, `"pr list"`, …).
    pub by_subcommand: BTreeMap<String, u64>,
    /// Count per originating source (cli / daemon / mcp).
    pub by_source: BTreeMap<Source, u64>,
}

impl GhCounts {
    /// Every `gh` invocation counted, including local (`--version`) probes.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.by_category.values().copied().sum()
    }

    /// GitHub **API** invocations — everything except local probes. This is the
    /// headline "calls omni-dev made" figure.
    #[must_use]
    pub fn api_total(&self) -> u64 {
        self.by_category
            .iter()
            .filter(|(cat, _)| **cat != Category::Local)
            .map(|(_, n)| *n)
            .sum()
    }

    /// A single compact line for the daemon's `tracing` summaries.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let sources: Vec<String> = self
            .by_source
            .iter()
            .map(|(s, n)| format!("{}={n}", source_str(*s)))
            .collect();
        let subs: Vec<String> = self
            .by_subcommand
            .iter()
            .map(|(name, n)| format!("{name}={n}"))
            .collect();
        let local = self.by_category.get(&Category::Local).copied().unwrap_or(0);
        format!(
            "{} api call(s) (total {}, local {local}); by source: [{}]; by subcommand: [{}]",
            self.api_total(),
            self.total(),
            sources.join(" "),
            subs.join(" "),
        )
    }

    /// Machine-readable form with string map keys (composes with `jq`). Shared
    /// by `omni-dev github count --json` and the daemon's `summary` op.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let by_category: BTreeMap<&str, u64> = self
            .by_category
            .iter()
            .map(|(c, n)| (c.as_str(), *n))
            .collect();
        let by_source: BTreeMap<&str, u64> = self
            .by_source
            .iter()
            .map(|(s, n)| (source_str(*s), *n))
            .collect();
        serde_json::json!({
            "api_total": self.api_total(),
            "total": self.total(),
            "by_category": by_category,
            "by_subcommand": self.by_subcommand,
            "by_source": by_source,
        })
    }

    fn tally(&mut self, command: &[String], source: Source) {
        *self
            .by_category
            .entry(Category::from_command(command))
            .or_default() += 1;
        *self.by_subcommand.entry(command.join(" ")).or_default() += 1;
        *self.by_source.entry(source).or_default() += 1;
    }
}

/// Aggregates `gh` records from the request log at `path` within the optional
/// `[since, until]` window and (optionally) restricted to one `source`.
///
/// Best-effort and tolerant like the rest of the log stack: a missing file or
/// malformed/partial lines yield empty/skipped tallies rather than an error, so
/// callers (CLI, daemon) never fail on a bad log.
#[must_use]
pub fn aggregate(
    path: &Path,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    source: Option<Source>,
) -> GhCounts {
    let mut counts = GhCounts::default();
    let Ok(file) = std::fs::File::open(path) else {
        return counts; // no log yet → zero counts
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<LogRecord>(&line) else {
            continue; // skip malformed/partial lines, as the stream reader does
        };
        if rec.kind != RecordKind::Gh {
            continue;
        }
        let rec_source = rec.source.unwrap_or(Source::Unknown);
        if let Some(want) = source {
            if rec_source != want {
                continue;
            }
        }
        if since.is_some() || until.is_some() {
            let Some(ts) = parse_timestamp(&rec.timestamp) else {
                continue; // undateable record can't be windowed → drop it
            };
            if since.is_some_and(|s| ts < s) || until.is_some_and(|u| ts > u) {
                continue;
            }
        }
        counts.tally(&rec.command, rec_source);
    }
    counts
}

/// Parses a record's RFC3339 timestamp to UTC, or `None` when unparseable.
/// Shared with `omni-dev log count`'s generic aggregator so both windows records
/// identically.
pub(crate) fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().copied().map(String::from).collect()
    }

    #[test]
    fn category_from_command_classifies_api_subcommand_and_local() {
        assert_eq!(
            Category::from_command(&cmd(&["api", "graphql"])),
            Category::Api
        );
        assert_eq!(
            Category::from_command(&cmd(&["api", "rate_limit"])),
            Category::Api
        );
        assert_eq!(
            Category::from_command(&cmd(&["pr", "list"])),
            Category::Subcommand
        );
        assert_eq!(
            Category::from_command(&cmd(&["repo", "view"])),
            Category::Subcommand
        );
        assert_eq!(
            Category::from_command(&cmd(&["--version"])),
            Category::Local
        );
        assert_eq!(Category::from_command(&[]), Category::Subcommand);
    }

    /// Five `gh` records (2 api, 2 `pr list`, 1 local) plus a non-gh record and
    /// two malformed lines the aggregator must ignore.
    const SAMPLE: &[&str] = &[
        r#"{"kind":"gh","timestamp":"2026-07-21T10:00:00.000Z","command":["api","graphql"],"source":"daemon"}"#,
        r#"{"kind":"gh","timestamp":"2026-07-21T10:01:00.000Z","command":["api","rate_limit"],"source":"daemon"}"#,
        r#"{"kind":"gh","timestamp":"2026-07-21T10:02:00.000Z","command":["pr","list"],"source":"cli"}"#,
        r#"{"kind":"gh","timestamp":"2026-07-21T10:03:00.000Z","command":["pr","list"],"source":"cli"}"#,
        r#"{"kind":"gh","timestamp":"2026-07-21T10:04:00.000Z","command":["--version"],"source":"cli"}"#,
        r#"{"kind":"http","timestamp":"2026-07-21T10:05:00.000Z","service":"jira"}"#,
        "not json",
        "",
    ];

    fn write_log(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    fn utc(ts: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn aggregate_tallies_by_category_subcommand_and_source_ignoring_non_gh() {
        let f = write_log(SAMPLE);
        let counts = aggregate(f.path(), None, None, None);
        assert_eq!(counts.total(), 5); // 5 gh records; http + junk ignored
        assert_eq!(counts.api_total(), 4); // excludes the 1 local `--version`
        assert_eq!(counts.by_category[&Category::Api], 2);
        assert_eq!(counts.by_category[&Category::Subcommand], 2);
        assert_eq!(counts.by_category[&Category::Local], 1);
        assert_eq!(counts.by_subcommand["pr list"], 2);
        assert_eq!(counts.by_subcommand["api graphql"], 1);
        assert_eq!(counts.by_source[&Source::Daemon], 2);
        assert_eq!(counts.by_source[&Source::Cli], 3);
    }

    #[test]
    fn aggregate_filters_by_source() {
        let f = write_log(SAMPLE);
        let counts = aggregate(f.path(), None, None, Some(Source::Daemon));
        assert_eq!(counts.total(), 2);
        assert_eq!(counts.api_total(), 2);
        assert!(!counts.by_source.contains_key(&Source::Cli));
    }

    #[test]
    fn aggregate_filters_by_time_window() {
        let f = write_log(SAMPLE);
        // Only the two `pr list` records fall in [10:02, 10:03:30].
        let counts = aggregate(
            f.path(),
            Some(utc("2026-07-21T10:02:00.000Z")),
            Some(utc("2026-07-21T10:03:30.000Z")),
            None,
        );
        assert_eq!(counts.total(), 2);
        assert_eq!(counts.by_subcommand["pr list"], 2);
    }

    #[test]
    fn aggregate_missing_file_is_empty() {
        let counts = aggregate(
            std::path::Path::new("/nonexistent/omni-dev/log.jsonl"),
            None,
            None,
            None,
        );
        assert_eq!(counts, GhCounts::default());
        assert_eq!(counts.total(), 0);
    }

    #[test]
    fn source_str_covers_every_source() {
        assert_eq!(source_str(Source::Cli), "cli");
        assert_eq!(source_str(Source::Mcp), "mcp");
        assert_eq!(source_str(Source::Daemon), "daemon");
        assert_eq!(source_str(Source::Unknown), "unknown");
    }

    #[test]
    fn aggregate_drops_undateable_record_when_windowed() {
        // A gh record whose timestamp cannot be parsed can't be placed in or out
        // of a window, so a windowed query drops it — while an unwindowed query
        // still counts it.
        let lines = &[
            r#"{"kind":"gh","timestamp":"not-a-timestamp","command":["pr","list"],"source":"cli"}"#,
            r#"{"kind":"gh","timestamp":"2026-07-21T10:02:00.000Z","command":["pr","list"],"source":"cli"}"#,
        ];
        let f = write_log(lines);
        let windowed = aggregate(
            f.path(),
            Some(utc("2026-07-21T00:00:00.000Z")),
            Some(utc("2026-07-22T00:00:00.000Z")),
            None,
        );
        assert_eq!(windowed.total(), 1); // only the dateable record survives
        assert_eq!(aggregate(f.path(), None, None, None).total(), 2);
    }

    #[test]
    fn to_json_has_string_keys_and_totals() {
        let f = write_log(SAMPLE);
        let v = aggregate(f.path(), None, None, None).to_json();
        assert_eq!(v["api_total"], 4);
        assert_eq!(v["total"], 5);
        assert_eq!(v["by_category"]["api"], 2);
        assert_eq!(v["by_category"]["local"], 1);
        assert_eq!(v["by_subcommand"]["pr list"], 2);
        assert_eq!(v["by_source"]["daemon"], 2);
    }

    #[test]
    fn summary_line_mentions_api_total() {
        let f = write_log(SAMPLE);
        let s = aggregate(f.path(), None, None, None).summary_line();
        assert!(s.contains("4 api call"), "summary was: {s}");
    }
}
