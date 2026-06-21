//! `omni-dev transcript youtube sync` — enumerate a channel's videos and
//! sync their transcripts to the filesystem, incrementally.
//!
//! Builds on the per-video fetcher: enumerate video IDs in each channel
//! ([`Youtube::recent_channel_videos`] for incremental, [`Youtube::all_channel_video_ids`]
//! for `--full` backfill), then loop the existing [`Youtube::fetch`], writing
//! each transcript to a deterministic path `<out>/<channel-id>/<video-id>.<lang>.<format>`.
//! "Already synced" is filesystem state: the target path existing means skip.
//!
//! Videos without a usable transcript (no captions, age-gated, region-locked)
//! are recorded and skipped — never abort the whole run.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Months, NaiveDate, Utc};
use clap::Parser;
use futures::stream::{self, StreamExt};

use crate::cli::transcript::format::CliFormat;
use crate::transcript::error::TranscriptError;
use crate::transcript::format::Format;
use crate::transcript::source::{FetchOpts, TranscriptSource};
use crate::transcript::sources::youtube::{metadata, Youtube};

/// Syncs transcripts for all videos in one or more YouTube channels to the
/// filesystem, incrementally (skips videos already synced).
#[derive(Parser)]
pub struct SyncCommand {
    /// One or more channel references: a `UC…` ID, a channel URL
    /// (`/channel/UC…`, `/@handle`, `/c/Name`, `/user/Name`), or a bare
    /// `@handle`.
    #[arg(required = true, num_args = 1..)]
    pub channels: Vec<String>,

    /// Output directory. Transcripts are written under
    /// `<out>/<channel-id>/<video-id>.<lang>.<format>`.
    #[arg(long)]
    pub out: PathBuf,

    /// Preferred caption language (e.g. `en`, `en-US`). Prefix fallback is
    /// applied — `en` matches `en-US`.
    #[arg(long, default_value = "en")]
    pub lang: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = CliFormat::Srt)]
    pub format: CliFormat,

    /// Allow falling through to auto-generated (ASR) captions when no manual
    /// track matches. Recommended for channel syncs — many uploads only have
    /// auto captions.
    #[arg(long)]
    pub auto: bool,

    /// Backfill the channel's entire upload history via the InnerTube
    /// `/browse` endpoint. Without this, only the ~15 most recent uploads
    /// (the RSS feed) are considered.
    #[arg(long)]
    pub full: bool,

    /// Only sync videos published on or after this date (`YYYY-MM-DD` or an
    /// RFC 3339 timestamp). Reliable for the default (RSS) path, which carries
    /// per-video publish dates; best-effort under `--full` (the browse grid
    /// has no per-item dates, so this is ignored there).
    #[arg(long, value_name = "DATE")]
    pub since: Option<String>,

    /// Maximum number of transcripts to fetch concurrently.
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    /// Re-fetch metadata sidecars whose `fetched_at` is older than this
    /// cutoff. Accepts `YYYY-MM-DD` (midnight UTC), a full RFC 3339 timestamp,
    /// or a relative spec like `"2 days ago"` (units: minute, hour, day, week,
    /// month, year). Without this flag existing sidecars are never refreshed,
    /// but missing ones are always downloaded.
    #[arg(long, value_name = "DATE_SPEC")]
    pub refresh_metadata_older_than: Option<String>,

    /// List what would be fetched without downloading or writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

impl SyncCommand {
    /// Runs the sync against the public YouTube origin.
    pub async fn execute(self) -> Result<()> {
        let yt = Youtube::new()?;
        self.sync_with(&yt).await
    }

    /// Plan, run, and report the sync against `yt`. Split from [`Self::execute`]
    /// so the whole orchestration is testable with a mock-backed [`Youtube`];
    /// `execute` only adds construction of the live client.
    async fn sync_with(self, yt: &Youtube) -> Result<()> {
        let plan = self.into_plan()?;
        let report = run(&plan, yt).await;
        report.print();
        Ok(())
    }

    /// Validate and normalise CLI args into a [`SyncPlan`].
    fn into_plan(self) -> Result<SyncPlan> {
        let since = self
            .since
            .as_deref()
            .map(parse_since)
            .transpose()
            .context("Invalid --since date")?;
        let refresh_cutoff = self
            .refresh_metadata_older_than
            .as_deref()
            .map(|s| parse_date_spec(s, Utc::now()))
            .transpose()
            .context("Invalid --refresh-metadata-older-than")?;
        Ok(SyncPlan {
            channels: self.channels,
            out: self.out,
            lang: self.lang,
            format: self.format,
            allow_auto: self.auto,
            full: self.full,
            since,
            refresh_cutoff,
            concurrency: self.concurrency.max(1),
            dry_run: self.dry_run,
        })
    }
}

/// Parse a `--since` value as either a bare `YYYY-MM-DD` date (interpreted as
/// midnight UTC) or a full RFC 3339 timestamp.
fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("expected YYYY-MM-DD or RFC 3339, got `{s}`"))?;
    #[allow(clippy::expect_used)]
    let naive = date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always a valid time");
    Ok(DateTime::from_naive_utc_and_offset(naive, Utc))
}

/// Parse a `--refresh-metadata-older-than` value into an absolute cutoff,
/// resolving relative forms against `now`.
///
/// Accepted forms:
/// - RFC 3339 timestamp (`2026-06-01T12:00:00+10:00`) — exact instant;
/// - `YYYY-MM-DD` (`2026-06-01`) — midnight UTC (consistent with `--since`);
/// - relative `<N> <unit>[s] ago` (`2 days ago`, `1 hour ago`) — units
///   minute, hour, day, week, month, year.
fn parse_date_spec(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        #[allow(clippy::expect_used)]
        let naive = date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time");
        return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
    }
    parse_relative(s, now)
        .with_context(|| format!("expected YYYY-MM-DD, RFC 3339, or `<N> <unit> ago`, got `{s}`"))
}

/// Resolve a relative `<N> <unit>[s] ago` spec against `now`. Months and years
/// use calendar arithmetic ([`chrono::Months`]); smaller units use fixed
/// [`chrono::Duration`] offsets.
fn parse_relative(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let lower = s.to_lowercase();
    let parts: Vec<&str> = lower.split_whitespace().collect();
    if parts.len() != 3 || parts[2] != "ago" {
        anyhow::bail!("not a relative date spec");
    }
    let n: i64 = parts[0]
        .parse()
        .with_context(|| format!("relative count `{}` is not an integer", parts[0]))?;
    if n < 0 {
        anyhow::bail!("relative count must be non-negative");
    }
    // Normalise an optional trailing plural, e.g. `days` -> `day`.
    let unit = parts[1].strip_suffix('s').unwrap_or(parts[1]);
    let cutoff = match unit {
        "minute" => now - Duration::minutes(n),
        "hour" => now - Duration::hours(n),
        "day" => now - Duration::days(n),
        "week" => now - Duration::weeks(n),
        "month" => now
            .checked_sub_months(Months::new(n as u32))
            .context("date underflow")?,
        "year" => now
            .checked_sub_months(Months::new(n as u32 * 12))
            .context("date underflow")?,
        other => anyhow::bail!("unknown relative unit `{other}`"),
    };
    Ok(cutoff)
}

/// Validated, normalised sync parameters (independent of the HTTP source so
/// the core loop is testable).
struct SyncPlan {
    channels: Vec<String>,
    out: PathBuf,
    lang: String,
    format: CliFormat,
    allow_auto: bool,
    full: bool,
    since: Option<DateTime<Utc>>,
    /// When set, metadata sidecars whose `fetched_at` predates this are
    /// re-fetched. `None` means missing sidecars are still backfilled but
    /// existing ones are never refreshed.
    refresh_cutoff: Option<DateTime<Utc>>,
    concurrency: usize,
    dry_run: bool,
}

/// Tally of a sync run, accumulated across all channels.
#[derive(Debug, Default, PartialEq, Eq)]
struct SyncReport {
    /// Transcripts newly written.
    synced: usize,
    /// Skipped because the target file already existed.
    already_present: usize,
    /// Skipped because the video has no usable transcript (no captions,
    /// age-gated, region-locked, …).
    no_transcript: usize,
    /// Failed for another reason (HTTP, parse, I/O).
    failed: usize,
    /// `--dry-run`: transcripts that would have been fetched.
    would_fetch: usize,
    /// Metadata sidecars newly written (backfill or for a freshly synced
    /// video).
    metadata_synced: usize,
    /// Existing metadata sidecars re-fetched because they were older than the
    /// `--refresh-metadata-older-than` cutoff.
    metadata_refreshed: usize,
    /// Metadata fetches/writes that failed. Tallied separately and never block
    /// or fail transcript syncing.
    metadata_failed: usize,
    /// `--dry-run`: metadata sidecars that would have been fetched.
    metadata_would_fetch: usize,
    /// Channels that could not be resolved or enumerated.
    channel_errors: usize,
}

impl SyncReport {
    fn print(&self) {
        println!("\nSync complete:");
        if self.would_fetch > 0 {
            println!("  would fetch:        {}", self.would_fetch);
        }
        if self.metadata_would_fetch > 0 {
            println!("  would fetch meta:   {}", self.metadata_would_fetch);
        }
        println!("  synced:             {}", self.synced);
        println!("  already present:    {}", self.already_present);
        println!("  no transcript:      {}", self.no_transcript);
        println!("  failed:             {}", self.failed);
        println!("  metadata synced:    {}", self.metadata_synced);
        println!("  metadata refreshed: {}", self.metadata_refreshed);
        println!("  metadata failed:    {}", self.metadata_failed);
        if self.channel_errors > 0 {
            println!("  channel errors:     {}", self.channel_errors);
        }
    }
}

/// Core sync loop. Resolves and enumerates each channel, plans which videos
/// need fetching (filesystem-as-state), then fetches the missing ones
/// concurrently. Never aborts on a single video or channel failure.
async fn run(plan: &SyncPlan, yt: &Youtube) -> SyncReport {
    let mut report = SyncReport::default();
    for channel in &plan.channels {
        if let Err(e) = sync_channel(plan, yt, channel, &mut report).await {
            eprintln!("channel `{channel}`: {e}");
            report.channel_errors += 1;
        }
    }
    report
}

/// Resolve, enumerate, and sync a single channel into `report`. Returns `Err`
/// only for channel-level failures (resolution / enumeration); per-video
/// failures are folded into `report`.
async fn sync_channel(
    plan: &SyncPlan,
    yt: &Youtube,
    channel: &str,
    report: &mut SyncReport,
) -> Result<(), TranscriptError> {
    let channel_id = yt.resolve_channel_id(channel).await?;
    let dir = plan.out.join(&channel_id);
    if !plan.dry_run {
        std::fs::create_dir_all(&dir)?;
    }

    let ids = enumerate(plan, yt, &channel_id).await?;
    let ext = Format::from(plan.format).as_str();
    let plan_result = plan_fetches(&ids, &dir, &plan.lang, ext, plan.full);
    report.already_present += plan_result.already_present;

    println!(
        "channel {channel_id}: {} video(s), {} to fetch, {} already present",
        ids.len(),
        plan_result.to_fetch.len(),
        plan_result.already_present,
    );

    if plan.dry_run {
        for (id, path) in &plan_result.to_fetch {
            println!("  would fetch {id} -> {}", path.display());
        }
        report.would_fetch += plan_result.to_fetch.len();

        // Optimistic preview: existing transcripts on disk plus the ones we
        // would sync this run, all assumed to gain a sidecar.
        let mut candidate_ids = scan_synced_ids(&dir);
        for (id, _) in &plan_result.to_fetch {
            if !candidate_ids.contains(id) {
                candidate_ids.push(id.clone());
            }
        }
        let meta_items = plan_metadata(&dir, &candidate_ids, plan.refresh_cutoff);
        for item in &meta_items {
            println!("  would fetch meta {} -> {}", item.id, item.path.display());
        }
        report.metadata_would_fetch += meta_items.len();
        return Ok(());
    }

    let opts = FetchOpts {
        language: plan.lang.clone(),
        allow_auto: plan.allow_auto,
        translate_to: None,
    };

    let outcomes = stream::iter(plan_result.to_fetch)
        .map(|(id, path)| {
            let opts = &opts;
            async move {
                let outcome = fetch_and_write(yt, &id, opts, plan.format, &path).await;
                (id, outcome)
            }
        })
        .buffer_unordered(plan.concurrency)
        .collect::<Vec<_>>()
        .await;

    for (id, outcome) in outcomes {
        match outcome {
            Ok(()) => report.synced += 1,
            Err(e) if is_no_transcript(&e) => {
                report.no_transcript += 1;
                eprintln!("  skip {id}: {e}");
            }
            Err(e) => {
                report.failed += 1;
                eprintln!("  fail {id}: {e}");
            }
        }
    }

    sync_metadata(plan, yt, &dir, report).await;
    Ok(())
}

/// Plan and write metadata sidecars for a channel directory. Scans the
/// directory *after* transcripts are written, so only videos with a transcript
/// on disk are considered (a video with no usable transcript leaves no anchor
/// file and gets no sidecar — see issue #976 open question 2). Backfills
/// missing sidecars and, when a refresh cutoff is set, re-fetches stale ones.
///
/// Metadata failures are tallied in `report` and never affect transcript
/// outcomes — they share the transcript `--concurrency` budget but run as a
/// separate pass.
async fn sync_metadata(plan: &SyncPlan, yt: &Youtube, dir: &Path, report: &mut SyncReport) {
    let candidate_ids = scan_synced_ids(dir);
    let meta_items = plan_metadata(dir, &candidate_ids, plan.refresh_cutoff);

    let outcomes = stream::iter(meta_items)
        .map(|item| async move {
            let outcome = fetch_and_write_metadata(yt, &item.id, &item.path).await;
            (item.reason, item.id, outcome)
        })
        .buffer_unordered(plan.concurrency)
        .collect::<Vec<_>>()
        .await;

    for (reason, id, outcome) in outcomes {
        match outcome {
            Ok(()) => match reason {
                MetaReason::Missing => report.metadata_synced += 1,
                MetaReason::Stale => report.metadata_refreshed += 1,
            },
            Err(e) => {
                report.metadata_failed += 1;
                eprintln!("  meta fail {id}: {e}");
            }
        }
    }
}

/// Enumerate a channel's video IDs, newest-first. `--full` pages the whole
/// upload history via browse; otherwise the RSS feed, filtered by `--since`.
async fn enumerate(
    plan: &SyncPlan,
    yt: &Youtube,
    channel_id: &str,
) -> Result<Vec<String>, TranscriptError> {
    if plan.full {
        yt.all_channel_video_ids(channel_id).await
    } else {
        let entries = yt.recent_channel_videos(channel_id).await?;
        Ok(entries
            .into_iter()
            .filter(|e| match (plan.since, e.published) {
                (Some(since), Some(published)) => published >= since,
                // No lower bound, or no date to compare against → keep it.
                _ => true,
            })
            .map(|e| e.id)
            .collect())
    }
}

/// Result of partitioning enumerated IDs against on-disk state.
struct FetchPlan {
    to_fetch: Vec<(String, PathBuf)>,
    already_present: usize,
}

/// Decide which enumerated videos need fetching. IDs are newest-first, so in
/// incremental mode (`full == false`) scanning stops at the first
/// already-present file: older uploads are assumed already synced, which
/// makes routine re-syncs cheap. Under `--full`, every ID is examined so gaps
/// in the history get filled.
fn plan_fetches(ids: &[String], dir: &Path, lang: &str, ext: &str, full: bool) -> FetchPlan {
    let mut to_fetch = Vec::new();
    let mut already_present = 0;
    for id in ids {
        let path = dir.join(format!("{id}.{lang}.{ext}"));
        if path.exists() {
            already_present += 1;
            if !full {
                break;
            }
        } else {
            to_fetch.push((id.clone(), path));
        }
    }
    FetchPlan {
        to_fetch,
        already_present,
    }
}

/// Fetch one transcript and write it atomically (temp file + rename) so a
/// partially written file is never mistaken for a completed sync.
async fn fetch_and_write(
    yt: &Youtube,
    id: &str,
    opts: &FetchOpts,
    format: CliFormat,
    path: &Path,
) -> Result<(), TranscriptError> {
    let transcript = yt.fetch(id, opts).await?;
    let rendered = Format::from(format).render(&transcript)?;
    write_atomic(path, &rendered)?;
    Ok(())
}

/// Write `contents` to `path` via a sibling temp file then rename, so readers
/// (including the next sync) never observe a half-written transcript.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Why a metadata sidecar is being (re)fetched. Drives which report counter
/// the outcome lands in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetaReason {
    /// No sidecar on disk, or its `fetched_at` was unparseable (corrupt).
    Missing,
    /// A sidecar exists but predates the `--refresh-metadata-older-than`
    /// cutoff.
    Stale,
}

/// A planned metadata sidecar write.
struct MetaItem {
    id: String,
    path: PathBuf,
    reason: MetaReason,
}

/// Distinct video IDs that have a transcript file in `dir`. Sidecars
/// (`*.meta.yaml`) and in-flight temp files (`.*` / `*.tmp`) are ignored;
/// every other file is a transcript output `<id>.<lang>.<ext>`, whose `<id>`
/// is the segment before the first `.` (YouTube IDs never contain one). A
/// missing directory yields no IDs.
fn scan_synced_ids(dir: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return ids;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || name.ends_with(".tmp") || name.ends_with(".meta.yaml") {
            continue;
        }
        let Some(id) = name.split('.').next().filter(|s| !s.is_empty()) else {
            continue;
        };
        if seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Decide which of `candidate_ids` need a metadata sidecar written. A sidecar
/// that is absent or whose `fetched_at` cannot be parsed (corrupt YAML /
/// missing key) is treated as [`MetaReason::Missing`]; a parseable one older
/// than `refresh_cutoff` (when set) is [`MetaReason::Stale`]. Anything else is
/// left untouched.
fn plan_metadata(
    dir: &Path,
    candidate_ids: &[String],
    refresh_cutoff: Option<DateTime<Utc>>,
) -> Vec<MetaItem> {
    let mut items = Vec::new();
    for id in candidate_ids {
        let path = dir.join(format!("{id}.meta.yaml"));
        let reason = match std::fs::read_to_string(&path) {
            // Absent or unreadable → backfill.
            Err(_) => Some(MetaReason::Missing),
            Ok(contents) => match metadata::read_fetched_at(&contents) {
                // Corrupt / no parseable fetched_at → treat as missing.
                None => Some(MetaReason::Missing),
                Some(fetched_at) => match refresh_cutoff {
                    Some(cutoff) if fetched_at < cutoff => Some(MetaReason::Stale),
                    _ => None,
                },
            },
        };
        if let Some(reason) = reason {
            items.push(MetaItem {
                id: id.clone(),
                path,
                reason,
            });
        }
    }
    items
}

/// Fetch a video's metadata via the un-gated WEB `/player` call and write the
/// sidecar atomically (temp file + rename), stamping `fetched_at` at fetch
/// time inside [`Youtube::fetch_video_metadata`].
async fn fetch_and_write_metadata(
    yt: &Youtube,
    id: &str,
    path: &Path,
) -> Result<(), TranscriptError> {
    let meta = yt.fetch_video_metadata(id).await?;
    let yaml = serde_yaml::to_string(&meta)
        .map_err(|e| TranscriptError::ParseError(format!("serialise metadata sidecar: {e}")))?;
    write_atomic(path, &yaml)?;
    Ok(())
}

/// Whether `err` means "this video simply has no transcript we can use" — a
/// skip-and-record condition rather than a run-aborting failure.
fn is_no_transcript(err: &TranscriptError) -> bool {
    matches!(
        err,
        TranscriptError::PlayabilityRefused { .. }
            | TranscriptError::LanguageNotFound { .. }
            | TranscriptError::AutoCaptionsRequireOptIn(_)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(args: &[&str]) -> SyncCommand {
        let cmd = SyncCommand::command().no_binary_name(true);
        let matches = cmd.try_get_matches_from(args).unwrap();
        SyncCommand::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn sync_command_defaults() {
        let cmd = parse(&["@handle", "--out", "/tmp/yt"]);
        assert_eq!(cmd.channels, vec!["@handle".to_string()]);
        assert_eq!(cmd.out, PathBuf::from("/tmp/yt"));
        assert_eq!(cmd.lang, "en");
        assert_eq!(cmd.format, CliFormat::Srt);
        assert!(!cmd.auto);
        assert!(!cmd.full);
        assert_eq!(cmd.since, None);
        assert_eq!(cmd.refresh_metadata_older_than, None);
        assert_eq!(cmd.concurrency, 4);
        assert!(!cmd.dry_run);
    }

    #[test]
    fn sync_command_all_flags_and_multiple_channels() {
        let cmd = parse(&[
            "UC_x5XG1OV2P6uZZ5FSM9Ttw",
            "@another",
            "--out",
            "/tmp/out",
            "--lang",
            "fr",
            "--format",
            "vtt",
            "--auto",
            "--full",
            "--since",
            "2024-01-01",
            "--refresh-metadata-older-than",
            "2 days ago",
            "--concurrency",
            "8",
            "--dry-run",
        ]);
        assert_eq!(cmd.channels.len(), 2);
        assert_eq!(cmd.lang, "fr");
        assert_eq!(cmd.format, CliFormat::Vtt);
        assert!(cmd.auto);
        assert!(cmd.full);
        assert_eq!(cmd.since.as_deref(), Some("2024-01-01"));
        assert_eq!(
            cmd.refresh_metadata_older_than.as_deref(),
            Some("2 days ago")
        );
        assert_eq!(cmd.concurrency, 8);
        assert!(cmd.dry_run);
    }

    #[test]
    fn sync_command_requires_a_channel() {
        let cmd = SyncCommand::command().no_binary_name(true);
        assert!(cmd.try_get_matches_from(["--out", "/tmp"]).is_err());
    }

    #[test]
    fn parse_since_accepts_date_and_rfc3339() {
        let date = parse_since("2024-11-20").unwrap();
        assert_eq!(date.to_rfc3339(), "2024-11-20T00:00:00+00:00");
        let ts = parse_since("2024-11-20T12:30:00+00:00").unwrap();
        assert_eq!(ts.to_rfc3339(), "2024-11-20T12:30:00+00:00");
    }

    #[test]
    fn parse_since_rejects_garbage() {
        assert!(parse_since("not-a-date").is_err());
    }

    fn now_fixed() -> DateTime<Utc> {
        "2026-06-21T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn parse_date_spec_accepts_absolute_forms() {
        let now = now_fixed();
        assert_eq!(
            parse_date_spec("2026-06-01", now).unwrap().to_rfc3339(),
            "2026-06-01T00:00:00+00:00"
        );
        assert_eq!(
            parse_date_spec("2026-06-01T12:00:00+10:00", now)
                .unwrap()
                .to_rfc3339(),
            "2026-06-01T02:00:00+00:00"
        );
    }

    #[test]
    fn parse_date_spec_resolves_relative_units_against_now() {
        let now = now_fixed();
        assert_eq!(
            parse_date_spec("30 minutes ago", now).unwrap().to_rfc3339(),
            "2026-06-21T11:30:00+00:00"
        );
        assert_eq!(
            parse_date_spec("2 hours ago", now).unwrap().to_rfc3339(),
            "2026-06-21T10:00:00+00:00"
        );
        assert_eq!(
            parse_date_spec("2 days ago", now).unwrap().to_rfc3339(),
            "2026-06-19T12:00:00+00:00"
        );
        assert_eq!(
            parse_date_spec("1 week ago", now).unwrap().to_rfc3339(),
            "2026-06-14T12:00:00+00:00"
        );
        // Calendar arithmetic for months/years.
        assert_eq!(
            parse_date_spec("3 months ago", now).unwrap().to_rfc3339(),
            "2026-03-21T12:00:00+00:00"
        );
        assert_eq!(
            parse_date_spec("1 year ago", now).unwrap().to_rfc3339(),
            "2025-06-21T12:00:00+00:00"
        );
    }

    #[test]
    fn parse_date_spec_accepts_singular_and_is_case_insensitive() {
        let now = now_fixed();
        assert_eq!(
            parse_date_spec("1 Day Ago", now).unwrap().to_rfc3339(),
            "2026-06-20T12:00:00+00:00"
        );
    }

    #[test]
    fn parse_date_spec_rejects_garbage() {
        let now = now_fixed();
        assert!(parse_date_spec("not-a-date", now).is_err());
        assert!(parse_date_spec("2 fortnights ago", now).is_err());
        assert!(parse_date_spec("yesterday", now).is_err());
        assert!(parse_date_spec("-1 days ago", now).is_err());
        assert!(parse_date_spec("2 days", now).is_err());
    }

    #[test]
    fn into_plan_clamps_zero_concurrency_to_one() {
        let cmd = parse(&["@h", "--out", "/tmp", "--concurrency", "0"]);
        let plan = cmd.into_plan().unwrap();
        assert_eq!(plan.concurrency, 1);
    }

    #[test]
    fn plan_fetches_incremental_stops_at_first_present() {
        let dir = tempfile::tempdir().unwrap();
        // Newest-first ids; mark the 2nd as already present.
        let ids: Vec<String> = vec!["aaa".into(), "bbb".into(), "ccc".into()];
        std::fs::write(dir.path().join("bbb.en.srt"), "x").unwrap();

        let plan = plan_fetches(ids.as_slice(), dir.path(), "en", "srt", false);
        // aaa is fetched; bbb is present → stop before ccc.
        assert_eq!(plan.already_present, 1);
        assert_eq!(
            plan.to_fetch
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            vec!["aaa".to_string()]
        );
    }

    #[test]
    fn plan_fetches_full_fills_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let ids: Vec<String> = vec!["aaa".into(), "bbb".into(), "ccc".into()];
        std::fs::write(dir.path().join("bbb.en.srt"), "x").unwrap();

        let plan = plan_fetches(ids.as_slice(), dir.path(), "en", "srt", true);
        // --full examines all: bbb present, aaa+ccc to fetch.
        assert_eq!(plan.already_present, 1);
        assert_eq!(
            plan.to_fetch
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            vec!["aaa".to_string(), "ccc".to_string()]
        );
    }

    #[test]
    fn plan_fetches_uses_lang_and_ext_in_path() {
        let dir = tempfile::tempdir().unwrap();
        let ids = vec!["zzz".to_string()];
        let plan = plan_fetches(&ids, dir.path(), "fr", "vtt", false);
        let (_, path) = &plan.to_fetch[0];
        assert!(path.ends_with("zzz.fr.vtt"));
    }

    #[test]
    fn write_atomic_writes_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vid.en.srt");
        write_atomic(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!dir.path().join(".vid.en.srt.tmp").exists());
    }

    #[test]
    fn is_no_transcript_classifies_skip_conditions() {
        assert!(is_no_transcript(&TranscriptError::PlayabilityRefused {
            status: "LOGIN_REQUIRED".into(),
            reason: None,
        }));
        assert!(is_no_transcript(&TranscriptError::LanguageNotFound {
            requested: "en".into(),
            available: vec![],
        }));
        assert!(is_no_transcript(
            &TranscriptError::AutoCaptionsRequireOptIn("en".into())
        ));
        assert!(!is_no_transcript(&TranscriptError::ParseError("x".into())));
    }

    // ── wiremock-driven end-to-end sync ──

    use serde_json::Value;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CHANNEL_ID: &str = "UC_x5XG1OV2P6uZZ5FSM9Ttw";
    const RSS_FEED: &str =
        include_str!("../../../transcript/sources/youtube/fixtures/channel_rss.xml");
    const PLAYER_RESPONSE: &str =
        include_str!("../../../transcript/sources/youtube/fixtures/player_response_basic.json");
    const PLAYER_RESPONSE_AGE_GATED: &str =
        include_str!("../../../transcript/sources/youtube/fixtures/player_response_age_gated.json");
    const TIMEDTEXT: &str =
        include_str!("../../../transcript/sources/youtube/fixtures/timedtext_basic.json");
    const WATCH_PAGE: &str = include_str!(
        "../../../transcript/sources/youtube/fixtures/watch_page_with_visitor_data.html"
    );
    const BROWSE_PAGE1: &str =
        include_str!("../../../transcript/sources/youtube/fixtures/browse_videos_page1.json");
    const BROWSE_PAGE2: &str =
        include_str!("../../../transcript/sources/youtube/fixtures/browse_videos_page2.json");
    const WEB_METADATA: &str = include_str!(
        "../../../transcript/sources/youtube/fixtures/player_response_web_metadata.json"
    );

    /// Point every caption track's `baseUrl` at the mock so `select_track`
    /// yields a URL the same server answers (mirrors the youtube.rs test
    /// helper).
    fn player_response_for(mock_uri: &str) -> String {
        let mut value: Value = serde_json::from_str(PLAYER_RESPONSE).unwrap();
        let tracks = value["captions"]["playerCaptionsTracklistRenderer"]["captionTracks"]
            .as_array_mut()
            .unwrap();
        for track in tracks {
            let lang = track["languageCode"].as_str().unwrap().to_string();
            track["baseUrl"] = Value::String(format!("{mock_uri}/api/timedtext?lang={lang}"));
        }
        serde_json::to_string(&value).unwrap()
    }

    async fn mount_rss(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/feeds/videos.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(RSS_FEED))
            .mount(server)
            .await;
    }

    async fn mount_watch(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(WATCH_PAGE))
            .mount(server)
            .await;
    }

    /// Mount a `/player` mock returning `body` for every video, plus the
    /// timedtext endpoint its caption URLs point at.
    async fn mount_player_body(server: &MockServer, body: String) {
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/player"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TIMEDTEXT))
            .mount(server)
            .await;
    }

    /// Mount the two-page `/browse` continuation sequence.
    async fn mount_browse(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/browse"))
            .and(body_partial_json(
                serde_json::json!({ "browseId": CHANNEL_ID }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(BROWSE_PAGE1))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/browse"))
            .and(body_partial_json(
                serde_json::json!({ "continuation": "CONT_TOKEN_1" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(BROWSE_PAGE2))
            .mount(server)
            .await;
    }

    /// Full happy-path server: RSS + watch + basic player + timedtext.
    async fn mock_youtube() -> (MockServer, Youtube) {
        let server = MockServer::start().await;
        mount_rss(&server).await;
        mount_watch(&server).await;
        mount_player_body(&server, player_response_for(&server.uri())).await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();
        (server, yt)
    }

    /// Mount the `/player` endpoint as two client-keyed mocks: the `ANDROID_VR`
    /// transcript call gets the basic caption fixture; the `WEB` metadata call
    /// gets the rich microformat fixture. The matchers are mutually exclusive
    /// (`clientName`), so each request resolves to exactly one. Lets metadata
    /// tests assert sidecar *content* distinct from the transcript fixture.
    async fn mount_player_split(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/player"))
            .and(body_partial_json(
                serde_json::json!({ "context": { "client": { "clientName": "ANDROID_VR" } } }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(player_response_for(&server.uri())),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/player"))
            .and(body_partial_json(
                serde_json::json!({ "context": { "client": { "clientName": "WEB" } } }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(WEB_METADATA))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TIMEDTEXT))
            .mount(server)
            .await;
    }

    /// Happy-path server with the transcript and metadata `/player` calls
    /// answered by *distinct* fixtures.
    async fn mock_youtube_split() -> (MockServer, Youtube) {
        let server = MockServer::start().await;
        mount_rss(&server).await;
        mount_watch(&server).await;
        mount_player_split(&server).await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();
        (server, yt)
    }

    /// Channel directory for the standard `CHANNEL_ID`, created on disk.
    fn channel_dir(root: &Path) -> PathBuf {
        let dir = root.join(CHANNEL_ID);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plan_for(out: PathBuf) -> SyncPlan {
        SyncPlan {
            channels: vec![CHANNEL_ID.to_string()],
            out,
            lang: "en".to_string(),
            format: CliFormat::Srt,
            allow_auto: false,
            full: false,
            since: None,
            refresh_cutoff: None,
            concurrency: 2,
            dry_run: false,
        }
    }

    #[tokio::test]
    async fn run_writes_transcripts_then_skips_on_resync() {
        let (_server, yt) = mock_youtube().await;
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_for(dir.path().to_path_buf());

        // First run: all 3 RSS entries are new.
        let report = run(&plan, &yt).await;
        assert_eq!(report.synced, 3);
        assert_eq!(report.already_present, 0);
        assert_eq!(report.failed, 0);

        let channel_dir = dir.path().join(CHANNEL_ID);
        for id in ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"] {
            assert!(channel_dir.join(format!("{id}.en.srt")).exists());
        }

        // Second run: newest is already present → incremental early-stop.
        let report2 = run(&plan, &yt).await;
        assert_eq!(report2.synced, 0);
        assert_eq!(report2.already_present, 1);
    }

    #[tokio::test]
    async fn run_dry_run_writes_nothing() {
        let (_server, yt) = mock_youtube().await;
        let dir = tempfile::tempdir().unwrap();
        let mut plan = plan_for(dir.path().to_path_buf());
        plan.dry_run = true;

        let report = run(&plan, &yt).await;
        assert_eq!(report.would_fetch, 3);
        assert_eq!(report.synced, 0);
        // No channel directory is created in dry-run.
        assert!(!dir.path().join(CHANNEL_ID).exists());
    }

    #[tokio::test]
    async fn run_full_enumerates_via_browse() {
        // --full takes the browse path (not RSS), paging both fixture pages.
        let server = MockServer::start().await;
        mount_watch(&server).await;
        mount_browse(&server).await;
        mount_player_body(&server, player_response_for(&server.uri())).await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut plan = plan_for(dir.path().to_path_buf());
        plan.full = true;

        let report = run(&plan, &yt).await;
        assert_eq!(report.synced, 3);
        let channel_dir = dir.path().join(CHANNEL_ID);
        for id in ["vid00000001", "vid00000002", "vid00000003"] {
            assert!(channel_dir.join(format!("{id}.en.srt")).exists());
        }
    }

    #[tokio::test]
    async fn run_since_filters_out_older_entries() {
        // RSS entries are dated 2024-11-24 / -20 / -15; a 2024-11-21 lower
        // bound keeps only the newest.
        let (_server, yt) = mock_youtube().await;
        let dir = tempfile::tempdir().unwrap();
        let mut plan = plan_for(dir.path().to_path_buf());
        plan.since = Some(parse_since("2024-11-21").unwrap());

        let report = run(&plan, &yt).await;
        assert_eq!(report.synced, 1);
        assert!(dir
            .path()
            .join(CHANNEL_ID)
            .join("aaaaaaaaaaa.en.srt")
            .exists());
    }

    #[tokio::test]
    async fn run_records_no_transcript_for_age_gated() {
        // Age-gated player response → PlayabilityRefused → skip-and-record.
        let server = MockServer::start().await;
        mount_rss(&server).await;
        mount_watch(&server).await;
        mount_player_body(&server, PLAYER_RESPONSE_AGE_GATED.to_string()).await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let report = run(&plan_for(dir.path().to_path_buf()), &yt).await;
        assert_eq!(report.no_transcript, 3);
        assert_eq!(report.synced, 0);
        assert_eq!(report.failed, 0);
    }

    #[tokio::test]
    async fn run_records_failures_on_http_error() {
        // Player 500 → HTTP error → recorded as a failure, run continues.
        let server = MockServer::start().await;
        mount_rss(&server).await;
        mount_watch(&server).await;
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/player"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let report = run(&plan_for(dir.path().to_path_buf()), &yt).await;
        assert_eq!(report.failed, 3);
        assert_eq!(report.synced, 0);
    }

    #[tokio::test]
    async fn run_records_channel_error_when_unresolvable() {
        // Channel page carries no channelId → ChannelNotFound → run records a
        // channel-level error and keeps going.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/@ghost"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>no id</html>"))
            .mount(&server)
            .await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut plan = plan_for(dir.path().to_path_buf());
        plan.channels = vec!["@ghost".to_string()];

        let report = run(&plan, &yt).await;
        assert_eq!(report.channel_errors, 1);
        assert_eq!(report.synced, 0);
    }

    #[tokio::test]
    async fn sync_with_drives_full_orchestration() {
        // Covers the execute() orchestration (into_plan → run → print) against
        // a mock-backed client, without constructing a live Youtube.
        let (_server, yt) = mock_youtube().await;
        let dir = tempfile::tempdir().unwrap();
        let cmd = SyncCommand {
            channels: vec![CHANNEL_ID.to_string()],
            out: dir.path().to_path_buf(),
            lang: "en".to_string(),
            format: CliFormat::Srt,
            auto: false,
            full: false,
            since: None,
            refresh_metadata_older_than: None,
            concurrency: 2,
            dry_run: false,
        };
        cmd.sync_with(&yt).await.unwrap();
        assert!(dir
            .path()
            .join(CHANNEL_ID)
            .join("aaaaaaaaaaa.en.srt")
            .exists());
    }

    #[tokio::test]
    async fn sync_with_surfaces_invalid_since() {
        // The into_plan() error path inside the orchestration.
        let (_server, yt) = mock_youtube().await;
        let dir = tempfile::tempdir().unwrap();
        let cmd = SyncCommand {
            channels: vec![CHANNEL_ID.to_string()],
            out: dir.path().to_path_buf(),
            lang: "en".to_string(),
            format: CliFormat::Srt,
            auto: false,
            full: false,
            since: Some("not-a-date".to_string()),
            refresh_metadata_older_than: None,
            concurrency: 2,
            dry_run: false,
        };
        let err = cmd.sync_with(&yt).await.unwrap_err();
        assert!(err.to_string().contains("Invalid --since date"));
    }

    // ── metadata sidecars ──

    #[tokio::test]
    async fn run_writes_metadata_sidecars_alongside_transcripts() {
        let (_server, yt) = mock_youtube_split().await;
        let dir = tempfile::tempdir().unwrap();
        let report = run(&plan_for(dir.path().to_path_buf()), &yt).await;

        assert_eq!(report.synced, 3);
        assert_eq!(report.metadata_synced, 3);
        assert_eq!(report.metadata_refreshed, 0);
        assert_eq!(report.metadata_failed, 0);

        let cdir = dir.path().join(CHANNEL_ID);
        for id in ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"] {
            let sidecar = cdir.join(format!("{id}.meta.yaml"));
            assert!(sidecar.exists(), "missing sidecar for {id}");
            let body = std::fs::read_to_string(&sidecar).unwrap();
            // Content comes from the WEB metadata fixture, not the transcript one.
            assert!(body.contains("schema: 1"));
            assert!(body.contains("category: Music"));
            assert!(body.contains("like_count: 19148727"));
            assert!(body.contains("fetched_at:"));
        }
    }

    #[tokio::test]
    async fn run_backfills_missing_sidecars_without_refetching_transcripts() {
        // Simulate a directory synced by an older version: transcripts on disk,
        // no sidecars. The incremental early-stop skips transcript fetches, but
        // the filesystem scan backfills every sidecar.
        let (_server, yt) = mock_youtube_split().await;
        let dir = tempfile::tempdir().unwrap();
        let cdir = channel_dir(dir.path());
        for id in ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"] {
            std::fs::write(cdir.join(format!("{id}.en.srt")), "x").unwrap();
        }

        let report = run(&plan_for(dir.path().to_path_buf()), &yt).await;

        assert_eq!(report.synced, 0, "transcripts already present");
        assert_eq!(report.metadata_synced, 3, "all sidecars backfilled");
        for id in ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"] {
            assert!(cdir.join(format!("{id}.meta.yaml")).exists());
        }
    }

    #[tokio::test]
    async fn run_refreshes_stale_sidecar_only_with_flag() {
        let (_server, yt) = mock_youtube_split().await;
        let dir = tempfile::tempdir().unwrap();
        let cdir = channel_dir(dir.path());
        std::fs::write(cdir.join("aaaaaaaaaaa.en.srt"), "x").unwrap();
        let sidecar = cdir.join("aaaaaaaaaaa.meta.yaml");
        let stale = "schema: 1\nvideo_id: aaaaaaaaaaa\ntitle: stale\n\
                     is_live_content: false\nfetched_at: \"2000-01-01T00:00:00Z\"\n";
        std::fs::write(&sidecar, stale).unwrap();

        // No flag → existing sidecar is left untouched.
        let report = run(&plan_for(dir.path().to_path_buf()), &yt).await;
        assert_eq!(report.metadata_refreshed, 0);
        assert_eq!(report.metadata_synced, 0);
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), stale);

        // Cutoff newer than the sidecar's fetched_at → refreshed.
        let mut plan = plan_for(dir.path().to_path_buf());
        plan.refresh_cutoff = Some("2026-01-01T00:00:00Z".parse().unwrap());
        let report = run(&plan, &yt).await;
        assert_eq!(report.metadata_refreshed, 1);
        assert_eq!(report.metadata_synced, 0);
        let refreshed = std::fs::read_to_string(&sidecar).unwrap();
        assert_ne!(refreshed, stale);
        assert!(refreshed.contains("category: Music"));
    }

    #[tokio::test]
    async fn run_treats_corrupt_sidecar_as_missing() {
        let (_server, yt) = mock_youtube_split().await;
        let dir = tempfile::tempdir().unwrap();
        let cdir = channel_dir(dir.path());
        std::fs::write(cdir.join("aaaaaaaaaaa.en.srt"), "x").unwrap();
        let sidecar = cdir.join("aaaaaaaaaaa.meta.yaml");
        // Parseable as YAML scalars but with no recoverable fetched_at.
        std::fs::write(&sidecar, "schema: 1\nvideo_id: aaaaaaaaaaa\n").unwrap();

        let report = run(&plan_for(dir.path().to_path_buf()), &yt).await;

        // Corrupt → treated as missing → rewritten and counted as synced.
        assert_eq!(report.metadata_synced, 1);
        assert_eq!(report.metadata_refreshed, 0);
        assert!(std::fs::read_to_string(&sidecar)
            .unwrap()
            .contains("category: Music"));
    }

    #[tokio::test]
    async fn run_metadata_failure_does_not_block_transcripts() {
        // Transcript path healthy (ANDROID_VR), metadata call (WEB) 500s.
        let server = MockServer::start().await;
        mount_rss(&server).await;
        mount_watch(&server).await;
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/player"))
            .and(body_partial_json(
                serde_json::json!({ "context": { "client": { "clientName": "ANDROID_VR" } } }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(player_response_for(&server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/player"))
            .and(body_partial_json(
                serde_json::json!({ "context": { "client": { "clientName": "WEB" } } }),
            ))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TIMEDTEXT))
            .mount(&server)
            .await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let report = run(&plan_for(dir.path().to_path_buf()), &yt).await;

        assert_eq!(report.synced, 3, "transcripts unaffected");
        assert_eq!(report.failed, 0);
        assert_eq!(report.metadata_failed, 3);
        assert_eq!(report.metadata_synced, 0);

        let cdir = dir.path().join(CHANNEL_ID);
        assert!(cdir.join("aaaaaaaaaaa.en.srt").exists());
        assert!(!cdir.join("aaaaaaaaaaa.meta.yaml").exists());
    }

    #[tokio::test]
    async fn run_dry_run_counts_sidecars_but_writes_nothing() {
        let (_server, yt) = mock_youtube_split().await;
        let dir = tempfile::tempdir().unwrap();
        let mut plan = plan_for(dir.path().to_path_buf());
        plan.dry_run = true;

        let report = run(&plan, &yt).await;
        assert_eq!(report.would_fetch, 3);
        assert_eq!(report.metadata_would_fetch, 3);
        assert_eq!(report.metadata_synced, 0);
        assert!(!dir.path().join(CHANNEL_ID).exists());
    }

    #[test]
    fn scan_synced_ids_ignores_sidecars_and_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("aaaaaaaaaaa.en.srt"), "x").unwrap();
        std::fs::write(p.join("aaaaaaaaaaa.fr.vtt"), "x").unwrap(); // same id, other lang
        std::fs::write(p.join("bbbbbbbbbbb.en.srt"), "x").unwrap();
        std::fs::write(p.join("bbbbbbbbbbb.meta.yaml"), "x").unwrap(); // sidecar
        std::fs::write(p.join(".ccccccccccc.en.srt.tmp"), "x").unwrap(); // in-flight temp

        let mut ids = scan_synced_ids(p);
        ids.sort();
        assert_eq!(
            ids,
            vec!["aaaaaaaaaaa".to_string(), "bbbbbbbbbbb".to_string()]
        );
    }

    #[test]
    fn plan_metadata_classifies_missing_stale_and_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // `miss` has no sidecar; `stale` and `fresh` do.
        std::fs::write(
            p.join("stale.meta.yaml"),
            "fetched_at: \"2000-01-01T00:00:00Z\"\n",
        )
        .unwrap();
        std::fs::write(
            p.join("fresh.meta.yaml"),
            "fetched_at: \"2026-12-31T00:00:00Z\"\n",
        )
        .unwrap();
        let cutoff: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
        let ids = vec!["miss".to_string(), "stale".to_string(), "fresh".to_string()];

        let items = plan_metadata(p, &ids, Some(cutoff));
        let by_id: std::collections::HashMap<_, _> =
            items.iter().map(|i| (i.id.as_str(), i.reason)).collect();
        assert_eq!(by_id.get("miss"), Some(&MetaReason::Missing));
        assert_eq!(by_id.get("stale"), Some(&MetaReason::Stale));
        assert_eq!(by_id.get("fresh"), None, "fresh sidecar is left alone");

        // Without a cutoff, only the missing one is planned.
        let items = plan_metadata(p, &ids, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "miss");
    }

    #[test]
    fn report_print_covers_optional_branches() {
        // Exercises the `would_fetch > 0` and `channel_errors > 0` branches.
        let report = SyncReport {
            synced: 1,
            already_present: 2,
            no_transcript: 3,
            failed: 4,
            would_fetch: 5,
            metadata_synced: 7,
            metadata_refreshed: 8,
            metadata_failed: 9,
            metadata_would_fetch: 10,
            channel_errors: 6,
        };
        report.print();
        SyncReport::default().print();
    }
}
