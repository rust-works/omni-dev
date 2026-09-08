//! Shared message selection over `manifest.jsonl` for `gmail insert`.
//!
//! `gmail render`'s only selector is `--archive-dir --all` (see
//! [`crate::cli::gmail::render`]) — there is no date/query/id filtering
//! anywhere else in `src/cli/gmail/**`, despite issue #1655 assuming `render`
//! already had one to reuse. This is deliberately **not** Gmail's search
//! query syntax: it filters records already captured locally in
//! `manifest.jsonl`, with no server round-trip, so it only needs a handful
//! of independent predicates rather than a query-language parser. Factored
//! out as its own module (rather than inlined into `insert.rs`) so `render`
//! can adopt it later — a follow-up, not part of this change.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use clap::Args;

use crate::cli::gmail::sync::manifest::ManifestRecord;

/// CLI flags selecting which archived messages `gmail insert` acts on.
///
/// Flattened into [`crate::cli::gmail::insert::InsertCommand`] via
/// `#[command(flatten)]`. At least one of `all`/`since`/`until`/`id`/
/// `ids_from`/`source_label` is required — enforced in
/// [`Selection::from_args`], not via a `clap::ArgGroup`, since `--id` is a
/// repeatable `Vec` whose "was this flag given at all" state clap groups
/// don't distinguish cleanly from "given with zero values". `--all` combined
/// with any other selector is also rejected there: it means "everything",
/// so pairing it with a narrowing filter is a contradiction worth failing
/// loudly on rather than silently picking a interpretation.
#[derive(Args, Debug, Clone, Default)]
pub struct SelectionArgs {
    /// Selects every non-deleted archived message.
    #[arg(long)]
    pub all: bool,

    /// Selects messages with an internal date on or after this UTC date
    /// (`YYYY-MM-DD`).
    #[arg(long, value_name = "DATE")]
    pub since: Option<String>,

    /// Selects messages with an internal date on or before this UTC date
    /// (`YYYY-MM-DD`), inclusive of the whole day.
    #[arg(long, value_name = "DATE")]
    pub until: Option<String>,

    /// Selects one specific archived message id. Repeatable.
    #[arg(long = "id", value_name = "ID")]
    pub ids: Vec<String>,

    /// Selects message ids listed one per line in FILE (`-` for stdin);
    /// blank lines and `#`-prefixed comments are skipped — the same shape a
    /// `--dry-run` report's ids can be piped back through.
    #[arg(long, value_name = "FILE")]
    pub ids_from: Option<PathBuf>,

    /// Selects messages carrying this Gmail label id in the archived
    /// manifest (a raw label id, e.g. `Label_1` — not the destination
    /// `--label` tag `gmail insert` itself applies).
    #[arg(long, value_name = "LABEL_ID")]
    pub source_label: Option<String>,
}

/// A validated, ready-to-apply selection — every fallible part of
/// [`SelectionArgs`] (date parsing, `--ids-from` file/stdin reads) is
/// resolved once up front in [`Self::from_args`], so a mistyped `--since`
/// fails in milliseconds rather than after hundreds of inserts.
#[derive(Debug, Default)]
pub(crate) struct Selection {
    all: bool,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    ids: HashSet<String>,
    source_label: Option<String>,
}

impl Selection {
    pub(crate) fn from_args(args: &SelectionArgs) -> Result<Self> {
        let mut ids: HashSet<String> = args.ids.iter().cloned().collect();
        if let Some(path) = &args.ids_from {
            ids.extend(read_ids_from(path)?);
        }

        let narrowing = args.since.is_some()
            || args.until.is_some()
            || !ids.is_empty()
            || args.source_label.is_some();
        anyhow::ensure!(
            !(args.all && narrowing),
            "--all cannot be combined with a narrowing selector (--since/--until/--id/\
             --ids-from/--source-label) — it already means \"everything\""
        );
        anyhow::ensure!(
            args.all || narrowing,
            "at least one selector is required: --all, --since, --until, --id, --ids-from, \
             or --source-label"
        );

        let since = args
            .since
            .as_deref()
            .map(parse_date_bound_start)
            .transpose()?;
        let until = args
            .until
            .as_deref()
            .map(parse_date_bound_end)
            .transpose()?;

        Ok(Self {
            all: args.all,
            since,
            until,
            ids,
            source_label: args.source_label.clone(),
        })
    }

    /// The explicit ids this selection names (via `--id`/`--ids-from`), for
    /// the engine to check against the manifest up front — an id absent
    /// from the archive is an error, never a silent drop.
    pub(crate) fn requested_ids(&self) -> &HashSet<String> {
        &self.ids
    }

    /// Whether `record` is selected — a plain conjunction of independent
    /// predicates: each is trivially satisfied when its corresponding
    /// option is unset, so specifying more than one narrows rather than
    /// widens the match set. A record with no parseable `internal_date` is
    /// excluded whenever `since`/`until` is set, and kept when neither is.
    pub(crate) fn matches(&self, record: &ManifestRecord) -> bool {
        if self.all {
            return true;
        }
        if !self.ids.is_empty() && !self.ids.contains(&record.id) {
            return false;
        }
        if let Some(label) = &self.source_label {
            if !record.label_ids.iter().any(|l| l == label) {
                return false;
            }
        }
        if self.since.is_some() || self.until.is_some() {
            let Some(date) = record.internal_date_utc() else {
                return false;
            };
            if self.since.is_some_and(|since| date < since) {
                return false;
            }
            if self.until.is_some_and(|until| date > until) {
                return false;
            }
        }
        true
    }
}

/// Parses `YYYY-MM-DD` as the first instant of that UTC day (`--since`'s
/// lower bound), mirroring `crate::cli::log::query::parse_time_bound`'s
/// date-only case.
fn parse_date_bound_start(s: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("invalid date {s:?} (expected YYYY-MM-DD)"))?;
    Ok(date.and_time(NaiveTime::MIN).and_utc())
}

/// Parses `YYYY-MM-DD` as the last instant of that UTC day (`--until`'s
/// upper bound), so `--until` is inclusive of the whole named day rather
/// than excluding everything after its midnight.
fn parse_date_bound_end(s: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("invalid date {s:?} (expected YYYY-MM-DD)"))?;
    let end_of_day = NaiveTime::from_hms_milli_opt(23, 59, 59, 999)
        .unwrap_or_else(|| unreachable!("23:59:59.999 is always a valid time"));
    Ok(date.and_time(end_of_day).and_utc())
}

/// Reads one id per line from `path` (`-` for stdin), skipping blank lines
/// and `#`-prefixed comments — the same shape a `--dry-run` report's ids can
/// be piped straight back into a follow-up `--ids-from -`.
fn read_ids_from(path: &std::path::Path) -> Result<Vec<String>> {
    let content = if path.as_os_str() == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .context("Failed to read ids from stdin")?;
        buf
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read ids from {}", path.display()))?
    };
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_record(id: &str) -> ManifestRecord {
        ManifestRecord {
            id: id.to_string(),
            thread_id: None,
            label_ids: Vec::new(),
            internal_date: None,
            subject: None,
            from: None,
            to: None,
            rfc822_msgid: None,
            in_reply_to: None,
            references: None,
            attachment_count: 0,
            attachment_filenames: Vec::new(),
            path: PathBuf::from(format!("messages/{id}.eml")),
            size: 0,
            history_id: None,
            deleted_at: None,
        }
    }

    fn dated_record(id: &str, ms: i64) -> ManifestRecord {
        ManifestRecord {
            internal_date: Some(ms.to_string()),
            ..sample_record(id)
        }
    }

    fn labeled_record(id: &str, labels: &[&str]) -> ManifestRecord {
        ManifestRecord {
            label_ids: labels
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            ..sample_record(id)
        }
    }

    // ── from_args validation ────────────────────────────────────────

    #[test]
    fn from_args_requires_at_least_one_selector() {
        let err = Selection::from_args(&SelectionArgs::default()).unwrap_err();
        assert!(err.to_string().contains("at least one selector"));
    }

    #[test]
    fn from_args_rejects_all_combined_with_a_narrowing_selector() {
        let args = SelectionArgs {
            all: true,
            since: Some("2026-01-01".to_string()),
            ..SelectionArgs::default()
        };
        let err = Selection::from_args(&args).unwrap_err();
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn from_args_accepts_all_alone() {
        assert!(Selection::from_args(&SelectionArgs {
            all: true,
            ..SelectionArgs::default()
        })
        .is_ok());
    }

    #[test]
    fn from_args_rejects_an_invalid_date() {
        let args = SelectionArgs {
            since: Some("not-a-date".to_string()),
            ..SelectionArgs::default()
        };
        let err = Selection::from_args(&args).unwrap_err();
        assert!(err.to_string().contains("invalid date"));
    }

    #[test]
    fn from_args_reads_ids_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ids.txt");
        std::fs::write(&path, "m1\n\n# a comment\nm2\n  m3  \n").unwrap();
        let args = SelectionArgs {
            ids_from: Some(path),
            ..SelectionArgs::default()
        };
        let selection = Selection::from_args(&args).unwrap();
        let mut ids: Vec<&str> = selection
            .requested_ids()
            .iter()
            .map(String::as_str)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["m1", "m2", "m3"]);
    }

    // ── matches (conjunction) ─────────────────────────────────────────

    #[test]
    fn matches_all_selects_everything() {
        let selection = Selection::from_args(&SelectionArgs {
            all: true,
            ..SelectionArgs::default()
        })
        .unwrap();
        assert!(selection.matches(&sample_record("m1")));
    }

    #[test]
    fn matches_id_selects_only_named_ids() {
        let args = SelectionArgs {
            ids: vec!["m1".to_string()],
            ..SelectionArgs::default()
        };
        let selection = Selection::from_args(&args).unwrap();
        assert!(selection.matches(&sample_record("m1")));
        assert!(!selection.matches(&sample_record("m2")));
    }

    #[test]
    fn matches_source_label_requires_the_label_present() {
        let args = SelectionArgs {
            source_label: Some("Label_1".to_string()),
            ..SelectionArgs::default()
        };
        let selection = Selection::from_args(&args).unwrap();
        assert!(selection.matches(&labeled_record("m1", &["Label_1"])));
        assert!(!selection.matches(&labeled_record("m2", &["Label_2"])));
    }

    #[test]
    fn matches_date_range_is_inclusive_on_both_ends() {
        let args = SelectionArgs {
            since: Some("2026-01-01".to_string()),
            until: Some("2026-01-02".to_string()),
            ..SelectionArgs::default()
        };
        let selection = Selection::from_args(&args).unwrap();
        let jan1_start = NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_time(NaiveTime::MIN)
            .and_utc()
            .timestamp_millis();
        let jan2_end = NaiveDate::from_ymd_opt(2026, 1, 2)
            .unwrap()
            .and_time(NaiveTime::from_hms_milli_opt(23, 59, 59, 999).unwrap())
            .and_utc()
            .timestamp_millis();
        let jan3_start = NaiveDate::from_ymd_opt(2026, 1, 3)
            .unwrap()
            .and_time(NaiveTime::MIN)
            .and_utc()
            .timestamp_millis();
        assert!(selection.matches(&dated_record("m1", jan1_start)));
        assert!(selection.matches(&dated_record("m2", jan2_end)));
        assert!(!selection.matches(&dated_record("m3", jan3_start)));
    }

    #[test]
    fn matches_date_range_excludes_records_with_no_parseable_date() {
        let args = SelectionArgs {
            since: Some("2026-01-01".to_string()),
            ..SelectionArgs::default()
        };
        let selection = Selection::from_args(&args).unwrap();
        assert!(!selection.matches(&sample_record("m1")));
    }

    #[test]
    fn matches_with_no_date_bound_keeps_records_with_no_date() {
        let selection = Selection::from_args(&SelectionArgs {
            all: true,
            ..SelectionArgs::default()
        })
        .unwrap();
        assert!(selection.matches(&sample_record("m1")));
    }

    #[test]
    fn matches_conjoins_id_and_source_label() {
        let args = SelectionArgs {
            ids: vec!["m1".to_string()],
            source_label: Some("Label_1".to_string()),
            ..SelectionArgs::default()
        };
        let selection = Selection::from_args(&args).unwrap();
        // Right id, wrong label -> excluded (conjunction, not either-or).
        assert!(!selection.matches(&labeled_record("m1", &["Label_2"])));
        assert!(selection.matches(&labeled_record("m1", &["Label_1"])));
    }
}
