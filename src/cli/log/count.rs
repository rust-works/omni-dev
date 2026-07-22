//! `omni-dev log count` — aggregate the request log by record kind and source.
//!
//! A generic per-kind tally over the same NDJSON log `omni-dev log` searches,
//! within an optional `--since`/`--until` window and optional `--source`/`--kind`
//! filters. Narrowed to `--kind gh` it renders the specialized GitHub breakdown
//! (by category, subcommand, and source) — the local, ground-truth counterpart
//! to `gh api rate_limit`'s server-side budget view (#1387).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::Parser;

use crate::github_metrics::{self, source_str, Category, GhCounts};
use crate::request_log::{self, LogRecord, RecordKind, Source};

/// Count records in the request log, grouped by kind and source.
///
/// Reads the same NDJSON log `omni-dev log` searches. With no `--kind` it tallies
/// every record by kind and source; `--kind gh` narrows to `gh` subprocess
/// records and renders the specialized GitHub breakdown — the local,
/// ground-truth counterpart to `gh api rate_limit`'s server-side view. Both
/// `gh api …` calls and higher-level subcommands (`gh pr list`, `gh repo view`,
/// …) are counted; local probes (`gh --version`) are tallied separately and
/// excluded from the API total.
#[derive(Parser)]
pub struct CountCommand {
    /// Lower time bound: relative (`30m`, `2h`, `1d`, `1w`) or absolute
    /// (`2026-07-01`, RFC3339). Same forms as `omni-dev log --since`.
    #[arg(long, value_name = "DUR_OR_TS")]
    since: Option<String>,
    /// Upper time bound (same forms as `--since`; a relative value means that
    /// long ago).
    #[arg(long, value_name = "DUR_OR_TS")]
    until: Option<String>,
    /// Restrict to records from one source.
    #[arg(long, value_name = "SOURCE")]
    source: Option<SourceArg>,
    /// Restrict to one record kind (`--kind gh` shows the GitHub breakdown).
    #[arg(long, value_name = "KIND")]
    kind: Option<KindArg>,
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
}

/// The three real sources a user can filter on (`Source::Unknown` is not a
/// selectable value).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
enum SourceArg {
    Cli,
    Daemon,
    Mcp,
}

impl From<SourceArg> for Source {
    fn from(s: SourceArg) -> Self {
        match s {
            SourceArg::Cli => Self::Cli,
            SourceArg::Daemon => Self::Daemon,
            SourceArg::Mcp => Self::Mcp,
        }
    }
}

/// The record kinds a user can filter on (`RecordKind::Unknown` is not a
/// selectable value).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
enum KindArg {
    Invocation,
    Http,
    Gh,
    Worktree,
}

impl From<KindArg> for RecordKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Invocation => Self::Invocation,
            KindArg::Http => Self::Http,
            KindArg::Gh => Self::Gh,
            KindArg::Worktree => Self::Worktree,
        }
    }
}

impl CountCommand {
    /// Aggregates the request log and prints the counters.
    pub fn execute(self) -> Result<()> {
        let path = request_log::log_file_path();
        // `render` already emits its own trailing newline (the JSON case) or a
        // newline-terminated table, so `print!` — not `println!` — avoids a
        // spurious blank line.
        print!("{}", self.render(path.as_deref())?);
        Ok(())
    }

    /// Aggregates the log at `path` (or an empty tally when it is `None`) and
    /// renders the result. Split out from [`Self::execute`] so the parsing,
    /// gh-vs-generic dispatch, and table-vs-JSON rendering are unit-testable
    /// against a temp log file without touching the ambient log path or stdout.
    fn render(self, path: Option<&Path>) -> Result<String> {
        // Reuse `omni-dev log`'s time-bound parser so `--since`/`--until` behave
        // identically to the search command.
        let since = self
            .since
            .as_deref()
            .map(super::parse_time_bound)
            .transpose()?;
        let until = self
            .until
            .as_deref()
            .map(super::parse_time_bound)
            .transpose()?;
        let source = self.source.map(Source::from);
        let kind = self.kind.map(RecordKind::from);

        // `--kind gh` renders the specialized GitHub breakdown; every other scope
        // uses the generic by-kind / by-source tally.
        let out = if kind == Some(RecordKind::Gh) {
            let counts = match path {
                Some(path) => github_metrics::aggregate(path, since, until, source),
                None => GhCounts::default(),
            };
            if self.json {
                format!("{}\n", serde_json::to_string_pretty(&counts.to_json())?)
            } else {
                gh_table(&counts)
            }
        } else {
            let counts = match path {
                Some(path) => aggregate(path, since, until, source, kind),
                None => LogCounts::default(),
            };
            if self.json {
                format!("{}\n", serde_json::to_string_pretty(&counts.to_json(kind))?)
            } else {
                counts.to_table(kind)
            }
        };
        Ok(out)
    }
}

/// Generic per-kind / per-source tallies over the request log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LogCounts {
    total: u64,
    by_kind: BTreeMap<RecordKind, u64>,
    by_source: BTreeMap<Source, u64>,
}

impl LogCounts {
    /// Renders the generic tally as a labelled table. `kind` is the active
    /// `--kind` filter (if any), shown in the header; a single-kind filter makes
    /// the per-kind section redundant, so it is dropped.
    fn to_table(&self, kind: Option<RecordKind>) -> String {
        let by_kind: BTreeMap<String, u64> = self
            .by_kind
            .iter()
            .map(|(k, n)| (k.as_str().to_string(), *n))
            .collect();
        let by_source: BTreeMap<String, u64> = self
            .by_source
            .iter()
            .map(|(s, n)| (source_str(*s).to_string(), *n))
            .collect();

        // One label column width across both sections, for aligned output.
        let pad = by_kind
            .keys()
            .chain(by_source.keys())
            .map(String::len)
            .max()
            .unwrap_or(0);

        let mut out = match kind {
            Some(k) => format!("Log records ({}): {}\n", k.as_str(), self.total),
            None => format!("Log records: {}\n", self.total),
        };
        if kind.is_none() {
            section("By kind", &by_kind, pad, &mut out);
        }
        section("By source", &by_source, pad, &mut out);
        out
    }

    /// Machine-readable form with string map keys (composes with `jq`). `kind`
    /// records the active `--kind` filter, or `null` when unfiltered.
    fn to_json(&self, kind: Option<RecordKind>) -> serde_json::Value {
        let by_kind: BTreeMap<&str, u64> =
            self.by_kind.iter().map(|(k, n)| (k.as_str(), *n)).collect();
        let by_source: BTreeMap<&str, u64> = self
            .by_source
            .iter()
            .map(|(s, n)| (source_str(*s), *n))
            .collect();
        serde_json::json!({
            "kind": kind.map(RecordKind::as_str),
            "total": self.total,
            "by_kind": by_kind,
            "by_source": by_source,
        })
    }
}

/// Streams the request log at `path`, tallying records by kind and source within
/// the optional `[since, until]` window and optional `source`/`kind` filters.
///
/// Best-effort like the rest of the log stack: a missing file or malformed lines
/// yield empty/skipped tallies rather than an error, so the command never fails
/// on a bad log.
fn aggregate(
    path: &Path,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    source: Option<Source>,
    kind: Option<RecordKind>,
) -> LogCounts {
    let mut counts = LogCounts::default();
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
        if let Some(want) = kind {
            if rec.kind != want {
                continue;
            }
        }
        let rec_source = rec.source.unwrap_or(Source::Unknown);
        if let Some(want) = source {
            if rec_source != want {
                continue;
            }
        }
        if since.is_some() || until.is_some() {
            let Some(ts) = github_metrics::parse_timestamp(&rec.timestamp) else {
                continue; // undateable record can't be windowed → drop it
            };
            if since.is_some_and(|s| ts < s) || until.is_some_and(|u| ts > u) {
                continue;
            }
        }
        counts.total += 1;
        *counts.by_kind.entry(rec.kind).or_default() += 1;
        *counts.by_source.entry(rec_source).or_default() += 1;
    }
    counts
}

/// A left-aligned `label  count` section; `pad` is the label column width.
fn section(title: &str, rows: &BTreeMap<String, u64>, pad: usize, out: &mut String) {
    out.push_str(&format!("\n{title}:\n"));
    if rows.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    for (label, n) in rows {
        out.push_str(&format!("  {label:<pad$}  {n}\n"));
    }
}

/// Renders the specialized GitHub breakdown for `--kind gh` (by category,
/// subcommand, and source) from the shared [`github_metrics`] aggregate.
fn gh_table(counts: &GhCounts) -> String {
    let local = counts
        .by_category
        .get(&Category::Local)
        .copied()
        .unwrap_or(0);

    let by_category: BTreeMap<String, u64> = counts
        .by_category
        .iter()
        .map(|(c, n)| (c.as_str().to_string(), *n))
        .collect();
    let by_source: BTreeMap<String, u64> = counts
        .by_source
        .iter()
        .map(|(s, n)| (source_str(*s).to_string(), *n))
        .collect();

    // One label column width across all three sections, for aligned output.
    let pad = by_category
        .keys()
        .chain(counts.by_subcommand.keys())
        .chain(by_source.keys())
        .map(String::len)
        .max()
        .unwrap_or(0);

    let mut out = format!(
        "GitHub API invocations: {}  (total gh calls: {}, local probes: {})\n",
        counts.api_total(),
        counts.total(),
        local,
    );
    section("By category", &by_category, pad, &mut out);
    section("By subcommand", &counts.by_subcommand, pad, &mut out);
    section("By source", &by_source, pad, &mut out);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Five valid records across three kinds (2 gh, 2 http, 1 invocation) plus
    /// two malformed lines the aggregator must ignore.
    const SAMPLE: &[&str] = &[
        r#"{"kind":"gh","timestamp":"2026-07-21T10:00:00.000Z","command":["api","graphql"],"source":"daemon"}"#,
        r#"{"kind":"gh","timestamp":"2026-07-21T10:01:00.000Z","command":["pr","list"],"source":"cli"}"#,
        r#"{"kind":"http","timestamp":"2026-07-21T10:02:00.000Z","service":"jira","source":"cli"}"#,
        r#"{"kind":"http","timestamp":"2026-07-21T10:03:00.000Z","service":"datadog","source":"daemon"}"#,
        r#"{"kind":"invocation","timestamp":"2026-07-21T10:04:00.000Z","source":"cli"}"#,
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
    fn aggregate_tallies_by_kind_and_source_ignoring_junk() {
        let f = write_log(SAMPLE);
        let counts = aggregate(f.path(), None, None, None, None);
        assert_eq!(counts.total, 5); // 5 valid records; junk lines ignored
        assert_eq!(counts.by_kind[&RecordKind::Gh], 2);
        assert_eq!(counts.by_kind[&RecordKind::Http], 2);
        assert_eq!(counts.by_kind[&RecordKind::Invocation], 1);
        assert_eq!(counts.by_source[&Source::Cli], 3);
        assert_eq!(counts.by_source[&Source::Daemon], 2);
    }

    #[test]
    fn aggregate_filters_by_kind() {
        let f = write_log(SAMPLE);
        let counts = aggregate(f.path(), None, None, None, Some(RecordKind::Http));
        assert_eq!(counts.total, 2);
        assert_eq!(counts.by_kind.len(), 1);
        assert_eq!(counts.by_kind[&RecordKind::Http], 2);
    }

    #[test]
    fn aggregate_filters_by_source() {
        let f = write_log(SAMPLE);
        let counts = aggregate(f.path(), None, None, Some(Source::Daemon), None);
        assert_eq!(counts.total, 2);
        assert!(!counts.by_source.contains_key(&Source::Cli));
    }

    #[test]
    fn aggregate_filters_by_time_window() {
        let f = write_log(SAMPLE);
        // Only the two http records fall in [10:02, 10:03:30].
        let counts = aggregate(
            f.path(),
            Some(utc("2026-07-21T10:02:00.000Z")),
            Some(utc("2026-07-21T10:03:30.000Z")),
            None,
            None,
        );
        assert_eq!(counts.total, 2);
        assert_eq!(counts.by_kind[&RecordKind::Http], 2);
    }

    #[test]
    fn aggregate_missing_file_is_empty() {
        let counts = aggregate(
            Path::new("/nonexistent/omni-dev/log.jsonl"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(counts, LogCounts::default());
        assert_eq!(counts.total, 0);
    }

    #[test]
    fn generic_json_has_string_keys_and_total() {
        let f = write_log(SAMPLE);
        let v = aggregate(f.path(), None, None, None, None).to_json(None);
        assert_eq!(v["total"], 5);
        assert_eq!(v["kind"], serde_json::Value::Null);
        assert_eq!(v["by_kind"]["gh"], 2);
        assert_eq!(v["by_kind"]["http"], 2);
        assert_eq!(v["by_source"]["cli"], 3);
    }

    #[test]
    fn generic_table_omits_by_kind_when_filtered() {
        let f = write_log(SAMPLE);
        let counts = aggregate(f.path(), None, None, None, Some(RecordKind::Http));
        let table = counts.to_table(Some(RecordKind::Http));
        assert!(
            table.contains("Log records (http): 2"),
            "table was: {table}"
        );
        assert!(!table.contains("By kind"), "table was: {table}");
        assert!(table.contains("By source"), "table was: {table}");
    }

    /// Builds a `CountCommand` with no time bounds; `render` is exercised against
    /// an explicit path, bypassing the ambient log-path resolution.
    fn cmd(source: Option<SourceArg>, kind: Option<KindArg>, json: bool) -> CountCommand {
        CountCommand {
            since: None,
            until: None,
            source,
            kind,
            json,
        }
    }

    #[test]
    fn render_gh_table_shows_the_github_breakdown() {
        let f = write_log(SAMPLE);
        let out = cmd(None, Some(KindArg::Gh), false)
            .render(Some(f.path()))
            .unwrap();
        assert!(out.contains("GitHub API invocations: 2"), "out was: {out}");
        assert!(out.contains("By category"), "out was: {out}");
        assert!(out.contains("api graphql"), "out was: {out}");
    }

    #[test]
    fn render_gh_json_is_valid_and_has_api_total() {
        let f = write_log(SAMPLE);
        let out = cmd(None, Some(KindArg::Gh), true)
            .render(Some(f.path()))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["api_total"], 2);
        assert_eq!(v["by_subcommand"]["pr list"], 1);
    }

    #[test]
    fn render_generic_table_lists_all_kinds() {
        let f = write_log(SAMPLE);
        let out = cmd(None, None, false).render(Some(f.path())).unwrap();
        assert!(out.contains("Log records: 5"), "out was: {out}");
        assert!(out.contains("By kind"), "out was: {out}");
        assert!(out.contains("By source"), "out was: {out}");
    }

    #[test]
    fn render_generic_json_is_valid_and_totals() {
        let f = write_log(SAMPLE);
        let out = cmd(None, None, true).render(Some(f.path())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total"], 5);
        assert_eq!(v["by_kind"]["gh"], 2);
    }

    #[test]
    fn render_source_filter_narrows_the_tally() {
        let f = write_log(SAMPLE);
        // Only the daemon-sourced gh + http records (2 total).
        let out = cmd(Some(SourceArg::Daemon), None, false)
            .render(Some(f.path()))
            .unwrap();
        assert!(out.contains("Log records: 2"), "out was: {out}");
    }

    #[test]
    fn render_with_no_log_path_reports_zero() {
        // Both branches degrade to an empty tally rather than erroring.
        let gh = cmd(None, Some(KindArg::Gh), false).render(None).unwrap();
        assert!(gh.contains("GitHub API invocations: 0"), "out was: {gh}");
        let generic = cmd(None, None, false).render(None).unwrap();
        assert!(generic.contains("Log records: 0"), "out was: {generic}");
    }

    #[test]
    fn render_rejects_a_bad_time_bound() {
        let bad = CountCommand {
            since: Some("not-a-time".to_string()),
            until: None,
            source: None,
            kind: None,
            json: false,
        };
        assert!(bad.render(None).is_err());
    }

    #[test]
    fn source_arg_converts_to_every_source() {
        assert_eq!(Source::from(SourceArg::Cli), Source::Cli);
        assert_eq!(Source::from(SourceArg::Daemon), Source::Daemon);
        assert_eq!(Source::from(SourceArg::Mcp), Source::Mcp);
    }

    #[test]
    fn kind_arg_converts_to_every_kind() {
        assert_eq!(
            RecordKind::from(KindArg::Invocation),
            RecordKind::Invocation
        );
        assert_eq!(RecordKind::from(KindArg::Http), RecordKind::Http);
        assert_eq!(RecordKind::from(KindArg::Gh), RecordKind::Gh);
    }
}
