//! `~/.omni-dev/voice/<id>/` session directory I/O.
//!
//! Lays out and reads the session directory format from #799:
//!
//! ```text
//! ~/.omni-dev/voice/<session-id>/
//!   meta.yaml          # session config (this issue: last_reflected_event_id + ttl defaults)
//!   transcript.jsonl   # append-only TranscriptEvent stream from `voice transcribe`
//!   events.jsonl       # append-only Event stream from `voice reflect` (and later `voice review`)
//!   reflections.log    # per-reflection summary line (cost, latency, status)
//! ```
//!
//! Shared with #804 (`voice review`), which reads the same `events.jsonl`
//! to produce materialised markdown projections. The session root path is
//! derived from `dirs::home_dir()` by default; the `OMNI_DEV_VOICE_ROOT`
//! environment variable overrides it (intended for tests, not a stable
//! user-facing knob).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::voice::events::Event;
use crate::voice::{EventId, TranscriptEvent};

/// Filesystem paths under a single session directory.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    /// Session root (`<voice-root>/<id>`).
    pub root: PathBuf,
    /// `meta.yaml` — session config (parsed into [`SessionMeta`]).
    pub meta: PathBuf,
    /// `transcript.jsonl` — `TranscriptEvent` log.
    pub transcript: PathBuf,
    /// `events.jsonl` — reflection [`Event`] log.
    pub events: PathBuf,
    /// `reflections.log` — per-reflection summary lines.
    pub log: PathBuf,
}

impl SessionPaths {
    /// Builds [`SessionPaths`] under `voice_root/<id>` without touching disk.
    #[must_use]
    pub fn under(voice_root: &Path, id: &str) -> Self {
        let root = voice_root.join(id);
        Self {
            meta: root.join("meta.yaml"),
            transcript: root.join("transcript.jsonl"),
            events: root.join("events.jsonl"),
            log: root.join("reflections.log"),
            root,
        }
    }
}

/// Default TTLs per item class (per #799), stored in `meta.yaml` so a
/// session can override them. Serialised as integer seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlDefaults {
    /// TTL for `class: todo` items.
    #[serde(with = "ttl_secs")]
    pub todo: Duration,
    /// TTL for `class: research` items.
    #[serde(with = "ttl_secs")]
    pub research: Duration,
    /// TTL for `class: question` items.
    #[serde(with = "ttl_secs")]
    pub question: Duration,
}

impl Default for TtlDefaults {
    fn default() -> Self {
        Self {
            todo: Duration::from_secs(7 * 86_400),
            research: Duration::from_secs(30 * 86_400),
            question: Duration::from_secs(14 * 86_400),
        }
    }
}

mod ttl_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

/// Parsed contents of `meta.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionMeta {
    /// `event_id` of the last `TranscriptEvent::Final` consumed by a
    /// previous `voice reflect` invocation. `None` until the first
    /// reflection completes.
    #[serde(default)]
    pub last_reflected_event_id: Option<EventId>,
    /// TTL defaults applied at projection time (consumed by #804).
    #[serde(default)]
    pub ttl_defaults: TtlDefaults,
}

/// Combination of paths and the parsed meta document.
#[derive(Debug, Clone)]
pub struct Session {
    /// On-disk paths under the session root.
    pub paths: SessionPaths,
    /// Parsed `meta.yaml` contents.
    pub meta: SessionMeta,
}

impl Session {
    /// Reads all `Final` transcript events from `transcript.jsonl` after
    /// `meta.last_reflected_event_id`. Non-`Final` events are skipped —
    /// reflection is driven by committed text only.
    pub fn read_transcript_finals_after(&self) -> Result<Vec<TranscriptEvent>> {
        read_transcript_finals_after(&self.paths.transcript, self.meta.last_reflected_event_id)
    }

    /// Reads `events.jsonl` into a [`Vec<Event>`]. Empty when the file
    /// doesn't exist or contains no events.
    pub fn read_events(&self) -> Result<Vec<Event>> {
        read_events(&self.paths.events)
    }

    /// Appends events to `events.jsonl`.
    pub fn append_events(&self, events: &[Event]) -> Result<()> {
        append_events(&self.paths.events, events)
    }

    /// Updates `meta.last_reflected_event_id` in memory and on disk.
    pub fn set_last_reflected(&mut self, id: EventId) -> Result<()> {
        self.meta.last_reflected_event_id = Some(id);
        write_meta(&self.paths.meta, &self.meta)
    }

    /// Appends a single line to `reflections.log` (no implicit newline).
    pub fn append_log(&self, line: &str) -> Result<()> {
        append_log_line(&self.paths.log, line)
    }
}

/// Resolves the session root: `$OMNI_DEV_VOICE_ROOT` if set, else
/// `~/.omni-dev/voice`.
pub fn voice_root() -> Result<PathBuf> {
    if let Ok(override_root) = std::env::var("OMNI_DEV_VOICE_ROOT") {
        return Ok(PathBuf::from(override_root));
    }
    let home = dirs::home_dir().context(
        "could not determine HOME directory for ~/.omni-dev/voice; \
         set OMNI_DEV_VOICE_ROOT to override",
    )?;
    Ok(home.join(".omni-dev").join("voice"))
}

/// Opens an existing session, or creates an empty one if the directory
/// doesn't exist. Bootstrap is idempotent: re-running against an
/// already-populated session reads the existing `meta.yaml`.
pub fn open_or_create(id: &str) -> Result<Session> {
    let root = voice_root()?;
    open_or_create_under(&root, id)
}

/// Variant of [`open_or_create`] that takes an explicit voice root —
/// useful for tests that drive several sessions under a `tempfile`
/// directory.
pub fn open_or_create_under(voice_root: &Path, id: &str) -> Result<Session> {
    if id.is_empty() {
        bail!("session id cannot be empty");
    }
    if id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        bail!("session id must not contain path separators: {id:?}");
    }
    let paths = SessionPaths::under(voice_root, id);
    std::fs::create_dir_all(&paths.root)
        .with_context(|| format!("creating session directory at {}", paths.root.display()))?;

    // Bootstrap empty files (touch only — don't truncate existing ones).
    for p in [&paths.transcript, &paths.events, &paths.log] {
        if !p.exists() {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .with_context(|| format!("creating {}", p.display()))?;
        }
    }

    let meta = if paths.meta.exists() {
        read_meta(&paths.meta)?
    } else {
        let m = SessionMeta::default();
        write_meta(&paths.meta, &m)?;
        m
    };

    Ok(Session { paths, meta })
}

/// Reads and parses `meta.yaml`.
pub fn read_meta(path: &Path) -> Result<SessionMeta> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading session meta at {}", path.display()))?;
    serde_yaml::from_str(&body)
        .with_context(|| format!("parsing session meta at {}", path.display()))
}

/// Writes `meta.yaml` atomically (write-temp-then-rename).
pub fn write_meta(path: &Path, meta: &SessionMeta) -> Result<()> {
    let body = serde_yaml::to_string(meta).context("serialising session meta to YAML")?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("writing temp meta at {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming temp meta to {}", path.display()))?;
    Ok(())
}

/// Reads all `TranscriptEvent`s from a JSONL file. Blank lines are
/// skipped; parse errors include the line number.
pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEvent>> {
    let file =
        File::open(path).with_context(|| format!("opening transcript at {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}:{}", path.display(), idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: TranscriptEvent = serde_json::from_str(&line)
            .with_context(|| format!("parsing {}:{}", path.display(), idx + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Reads only `Final` transcript events after the optional marker.
///
/// Uses stream position (not ULID comparison) — finds the marker line
/// and returns Finals strictly after it. Errors if the marker is set
/// but not present in the file.
pub fn read_transcript_finals_after(
    path: &Path,
    after: Option<EventId>,
) -> Result<Vec<TranscriptEvent>> {
    let all = read_transcript(path)?;
    let finals: Vec<TranscriptEvent> = all
        .into_iter()
        .filter(|e| matches!(e, TranscriptEvent::Final { .. }))
        .collect();
    match after {
        None => Ok(finals),
        Some(target) => {
            let pos = finals.iter().position(|e| match e {
                TranscriptEvent::Final { event_id, .. } => *event_id == target,
                _ => false,
            });
            match pos {
                Some(idx) => Ok(finals.into_iter().skip(idx + 1).collect()),
                None => bail!(
                    "last_reflected_event_id {target} not found in transcript at {}; \
                     meta.yaml may be inconsistent with transcript.jsonl",
                    path.display()
                ),
            }
        }
    }
}

/// Reads all reflection [`Event`]s from `events.jsonl`. Returns an empty
/// vec if the file doesn't exist (greenfield session).
pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file =
        File::open(path).with_context(|| format!("opening events log at {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}:{}", path.display(), idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Event = serde_json::from_str(&line)
            .with_context(|| format!("parsing {}:{}", path.display(), idx + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Appends events as JSONL to `path`. Each event is one line, flushed
/// after the batch. Skips silently when `events` is empty.
pub fn append_events(path: &Path, events: &[Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening events log for append at {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for e in events {
        serde_json::to_writer(&mut writer, e)
            .with_context(|| format!("serialising event to {}", path.display()))?;
        writer
            .write_all(b"\n")
            .with_context(|| format!("appending newline to {}", path.display()))?;
    }
    writer
        .flush()
        .with_context(|| format!("flushing events log at {}", path.display()))?;
    Ok(())
}

/// Appends a single line (with newline) to `reflections.log`. Creates
/// the file if it does not exist.
pub fn append_log_line(path: &Path, line: &str) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening reflections log at {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(line.as_bytes())
        .with_context(|| format!("writing log line to {}", path.display()))?;
    if !line.ends_with('\n') {
        writer
            .write_all(b"\n")
            .with_context(|| format!("appending newline to {}", path.display()))?;
    }
    writer
        .flush()
        .with_context(|| format!("flushing reflections log at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::events::{
        EventKind, ItemClass, ItemCreate, Provenance, ReflectionId, TranscriptSpan,
    };
    use crate::voice::transcriber::EndpointKind;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn fixed_ts() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn provenance() -> Provenance {
        Provenance {
            transcript_span: TranscriptSpan {
                start_event_id: ulid::Ulid::from_parts(0, 1),
                end_event_id: ulid::Ulid::from_parts(0, 2),
            },
            model: Some("m".into()),
            prompt_version: Some("p".into()),
        }
    }

    fn make_event(event_id: u128) -> Event {
        Event {
            event_id: ulid::Ulid::from_parts(0, event_id),
            ts: fixed_ts(),
            reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 100)),
            provenance: provenance(),
            kind: EventKind::ItemCreate(ItemCreate {
                item_id: ulid::Ulid::from_parts(0, 500),
                class: ItemClass::Todo,
                text: format!("event {event_id}"),
                priority: None,
                valid_until: None,
                tags: None,
            }),
        }
    }

    fn make_final(event_id: u128, text: &str) -> TranscriptEvent {
        TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, event_id),
            text: text.to_string(),
            start: Duration::from_millis(0),
            end: Duration::from_millis(100),
            confidence: 0.9,
            words: None,
            speaker: None,
            revisable: false,
        }
    }

    #[test]
    fn open_or_create_bootstraps_an_empty_session() {
        let tmp = TempDir::new().unwrap();
        let session = open_or_create_under(tmp.path(), "s1").unwrap();
        assert!(session.paths.meta.exists());
        assert!(session.paths.transcript.exists());
        assert!(session.paths.events.exists());
        assert!(session.paths.log.exists());
        assert_eq!(session.meta, SessionMeta::default());
    }

    #[test]
    fn open_or_create_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let s1 = open_or_create_under(tmp.path(), "s1").unwrap();
        let s2 = open_or_create_under(tmp.path(), "s1").unwrap();
        assert_eq!(s1.meta, s2.meta);
    }

    #[test]
    fn open_or_create_preserves_existing_meta() {
        let tmp = TempDir::new().unwrap();
        let mut s = open_or_create_under(tmp.path(), "s1").unwrap();
        s.set_last_reflected(ulid::Ulid::from_parts(0, 42)).unwrap();
        let reopened = open_or_create_under(tmp.path(), "s1").unwrap();
        assert_eq!(
            reopened.meta.last_reflected_event_id,
            Some(ulid::Ulid::from_parts(0, 42))
        );
    }

    #[test]
    fn rejects_session_id_with_path_separator() {
        let tmp = TempDir::new().unwrap();
        assert!(open_or_create_under(tmp.path(), "a/b").is_err());
        assert!(open_or_create_under(tmp.path(), "a\\b").is_err());
        assert!(open_or_create_under(tmp.path(), "..").is_err());
        assert!(open_or_create_under(tmp.path(), ".").is_err());
        assert!(open_or_create_under(tmp.path(), "").is_err());
    }

    #[test]
    fn ttl_defaults_match_799_defaults() {
        let t = TtlDefaults::default();
        assert_eq!(t.todo, Duration::from_secs(7 * 86_400));
        assert_eq!(t.research, Duration::from_secs(30 * 86_400));
        assert_eq!(t.question, Duration::from_secs(14 * 86_400));
    }

    #[test]
    fn meta_yaml_round_trip_preserves_optional_marker() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("meta.yaml");
        let meta = SessionMeta {
            last_reflected_event_id: Some(ulid::Ulid::from_parts(0, 7)),
            ttl_defaults: TtlDefaults::default(),
        };
        write_meta(&path, &meta).unwrap();
        let back = read_meta(&path).unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn append_then_read_events_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        append_events(&path, &[make_event(1), make_event(2)]).unwrap();
        let back = read_events(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], make_event(1));
        assert_eq!(back[1], make_event(2));
    }

    #[test]
    fn append_events_with_empty_slice_is_noop() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        append_events(&path, &[]).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn read_events_on_missing_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let result = read_events(&tmp.path().join("nothing.jsonl")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn read_transcript_finals_after_filters_partials_and_endpoints() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&TranscriptEvent::Partial {
                    text: "ignored".into(),
                    start: Duration::ZERO,
                    end: Duration::from_millis(50),
                    words: None,
                    speaker: None,
                })
                .unwrap(),
                serde_json::to_string(&make_final(1, "first")).unwrap(),
                serde_json::to_string(&TranscriptEvent::Endpoint {
                    at: Duration::from_secs(1),
                    kind: EndpointKind::StreamEnd,
                })
                .unwrap(),
            ),
        )
        .unwrap();
        let finals = read_transcript_finals_after(&path, None).unwrap();
        assert_eq!(finals.len(), 1);
    }

    #[test]
    fn read_transcript_finals_after_skips_through_marker() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        let lines = [
            serde_json::to_string(&make_final(1, "a")).unwrap(),
            serde_json::to_string(&make_final(2, "b")).unwrap(),
            serde_json::to_string(&make_final(3, "c")).unwrap(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let after_id = ulid::Ulid::from_parts(0, 2);
        let finals = read_transcript_finals_after(&path, Some(after_id)).unwrap();
        assert_eq!(finals.len(), 1);
        match &finals[0] {
            TranscriptEvent::Final { text, .. } => assert_eq!(text, "c"),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn read_transcript_finals_after_errors_when_marker_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            serde_json::to_string(&make_final(1, "a")).unwrap() + "\n",
        )
        .unwrap();
        let err =
            read_transcript_finals_after(&path, Some(ulid::Ulid::from_parts(0, 99))).unwrap_err();
        assert!(
            err.to_string().contains("not found in transcript"),
            "got: {err}"
        );
    }

    #[test]
    fn append_log_line_creates_file_and_adds_newline() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("reflections.log");
        append_log_line(&path, "first entry").unwrap();
        append_log_line(&path, "second entry\n").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "first entry\nsecond entry\n");
    }

    #[test]
    fn read_transcript_skips_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            format!(
                "\n{}\n\n   \n{}\n",
                serde_json::to_string(&make_final(1, "a")).unwrap(),
                serde_json::to_string(&make_final(2, "b")).unwrap(),
            ),
        )
        .unwrap();
        let events = read_transcript(&path).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn read_transcript_reports_parse_failure_with_line_number() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        let good = serde_json::to_string(&make_final(1, "ok")).unwrap();
        std::fs::write(&path, format!("{good}\nnot valid json\n")).unwrap();
        let err = read_transcript(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("parsing") && msg.contains(":2"),
            "error should point at line 2: {msg}"
        );
    }

    #[test]
    fn read_events_skips_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        append_events(&path, &[make_event(1)]).unwrap();
        // Add a blank line after the existing content.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "\n  ").unwrap();
        drop(f);
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn read_events_reports_parse_failure_with_line_number() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        std::fs::write(&path, "not valid json at all\n").unwrap();
        let err = read_events(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("parsing") && msg.contains(":1"),
            "error should point at line 1: {msg}"
        );
    }

    #[test]
    fn voice_root_respects_override_env_var() {
        // Env mutation is process-wide; restore on exit. No serial guard
        // here because no other test in this module reads/writes the var.
        let original = std::env::var("OMNI_DEV_VOICE_ROOT").ok();
        std::env::set_var("OMNI_DEV_VOICE_ROOT", "/tmp/overridden");
        let root = voice_root().unwrap();
        assert_eq!(root, PathBuf::from("/tmp/overridden"));
        match original {
            Some(v) => std::env::set_var("OMNI_DEV_VOICE_ROOT", v),
            None => std::env::remove_var("OMNI_DEV_VOICE_ROOT"),
        }
    }
}
