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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use futures::stream::{self, StreamExt};

use crate::cli::transcript::format::CliFormat;
use crate::transcript::error::TranscriptError;
use crate::transcript::format::Format;
use crate::transcript::source::{FetchOpts, TranscriptSource};
use crate::transcript::sources::youtube::Youtube;

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
        Ok(SyncPlan {
            channels: self.channels,
            out: self.out,
            lang: self.lang,
            format: self.format,
            allow_auto: self.auto,
            full: self.full,
            since,
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
    /// `--dry-run`: would have been fetched.
    would_fetch: usize,
    /// Channels that could not be resolved or enumerated.
    channel_errors: usize,
}

impl SyncReport {
    fn print(&self) {
        println!("\nSync complete:");
        if self.would_fetch > 0 {
            println!("  would fetch:      {}", self.would_fetch);
        }
        println!("  synced:           {}", self.synced);
        println!("  already present:  {}", self.already_present);
        println!("  no transcript:    {}", self.no_transcript);
        println!("  failed:           {}", self.failed);
        if self.channel_errors > 0 {
            println!("  channel errors:   {}", self.channel_errors);
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
    Ok(())
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

    fn plan_for(out: PathBuf) -> SyncPlan {
        SyncPlan {
            channels: vec![CHANNEL_ID.to_string()],
            out,
            lang: "en".to_string(),
            format: CliFormat::Srt,
            allow_auto: false,
            full: false,
            since: None,
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
            concurrency: 2,
            dry_run: false,
        };
        let err = cmd.sync_with(&yt).await.unwrap_err();
        assert!(err.to_string().contains("Invalid --since date"));
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
            channel_errors: 6,
        };
        report.print();
        SyncReport::default().print();
    }
}
