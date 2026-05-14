//! `voice review` driver — wraps the pure [`crate::voice::reconcile`]
//! function with the I/O the CLI needs.
//!
//! Responsibilities:
//! - Open the session under the configured voice root.
//! - For `What::Transcript`: stream `transcript.jsonl` through the
//!   existing markdown renderer to the caller's `Write`.
//! - For the other variants: call `reconcile()`, atomic-write the
//!   selected markdown file(s) under the session root, and append the
//!   synthesised TTL-expiry events to `events.jsonl`.
//!
//! The TTL pass runs whenever the selected mode reads `events.jsonl`
//! (`Todos`, `Decisions`, `All`) — even when only one markdown file
//! is being written, so the log stays consistent regardless of which
//! `--what` the user invokes.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::voice::clock::Clock;
use crate::voice::det::UlidRng;
use crate::voice::reconcile::reconcile;
use crate::voice::render::render_markdown;
use crate::voice::session::{self, Session};

/// Selects which artefacts the review command should materialise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum What {
    /// Render `transcript.jsonl` to stdout as markdown. No TTL pass.
    Transcript,
    /// Write `todos.md` under the session root. Runs TTL pass.
    Todos,
    /// Write `decisions.md` under the session root. Runs TTL pass.
    Decisions,
    /// Write both `todos.md` and `decisions.md`. Runs TTL pass.
    All,
}

/// Inputs for [`run_review`]. Mirrors [`crate::voice::reflect::ReflectOptions`]:
/// pluggable RNG and clock so tests can pin both.
pub struct ReviewOptions {
    /// Session id under the voice root.
    pub session_id: String,
    /// Which artefacts to materialise.
    pub what: What,
    /// ULID source for synthesised TTL-expiry events.
    pub ulid_rng: Box<dyn UlidRng>,
    /// Wall-clock source for `now` and synthesised event timestamps.
    pub clock: Box<dyn Clock>,
    /// Override the voice root directory (test hook). When `None` the
    /// standard `~/.omni-dev/voice/` root (or `OMNI_DEV_VOICE_ROOT`) is
    /// used.
    pub session_root_override: Option<PathBuf>,
}

/// Runs one `voice review` invocation end-to-end.
///
/// `stdout` is only written to for `What::Transcript`; the other
/// variants write files under the session root and leave `stdout`
/// untouched.
pub fn run_review<W: Write>(opts: ReviewOptions, stdout: &mut W) -> Result<()> {
    let ReviewOptions {
        session_id,
        what,
        mut ulid_rng,
        clock,
        session_root_override,
    } = opts;

    let voice_root = match session_root_override {
        Some(path) => path,
        None => session::voice_root()?,
    };
    let session = session::open_or_create_under(&voice_root, &session_id)?;

    match what {
        What::Transcript => render_transcript(&session, stdout),
        What::Todos | What::Decisions | What::All => {
            run_reconcile_and_write(&session, what, ulid_rng.as_mut(), clock.as_ref())
        }
    }
}

fn render_transcript<W: Write>(session: &Session, w: &mut W) -> Result<()> {
    let events = session::read_transcript(&session.paths.transcript).with_context(|| {
        format!(
            "reading transcript at {}",
            session.paths.transcript.display()
        )
    })?;
    render_markdown(events.into_iter().map(Ok), w)
}

fn run_reconcile_and_write(
    session: &Session,
    what: What,
    rng: &mut dyn UlidRng,
    clock: &dyn Clock,
) -> Result<()> {
    let events = session.read_events()?;
    let out = reconcile(&events, &session.meta.ttl_defaults, clock.now(), rng);

    let write_todos = matches!(what, What::Todos | What::All);
    let write_decisions = matches!(what, What::Decisions | What::All);

    if write_todos {
        let path = session.paths.root.join("todos.md");
        atomic_write(&path, out.todos_md.as_bytes())?;
    }
    if write_decisions {
        let path = session.paths.root.join("decisions.md");
        atomic_write(&path, out.decisions_md.as_bytes())?;
    }
    session.append_events(&out.new_expiry_events)?;
    Ok(())
}

/// Writes `bytes` to `path` atomically by way of `<path>.tmp` + rename.
///
/// Mirrors the temp-then-rename pattern used by
/// [`session::write_meta`] — keeps the in-tree convention consistent
/// and avoids the `NamedTempFile` dependency surface for what is just
/// a two-line operation.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(temp_extension(path));
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("writing temp file at {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming temp file to {}", path.display()))?;
    Ok(())
}

fn temp_extension(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::clock::FixedClock;
    use crate::voice::det::CountingUlidRng;
    use crate::voice::events::{Event, EventKind, ItemClass, ItemCreate, Provenance, ReflectionId};
    use tempfile::TempDir;

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
    }

    fn make_create(eid: u128, iid: u128, text: &str, ts: chrono::DateTime<chrono::Utc>) -> Event {
        Event {
            event_id: ulid::Ulid::from_parts(0, eid),
            ts,
            reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 100)),
            provenance: Provenance {
                transcript_span: None,
                model: None,
                prompt_version: None,
            },
            kind: EventKind::ItemCreate(ItemCreate {
                item_id: ulid::Ulid::from_parts(0, iid),
                class: ItemClass::Todo,
                text: text.into(),
                priority: None,
                valid_until: None,
                tags: None,
            }),
        }
    }

    fn build_opts(root: &Path, session_id: &str, what: What) -> ReviewOptions {
        ReviewOptions {
            session_id: session_id.into(),
            what,
            ulid_rng: Box::new(CountingUlidRng::new()),
            clock: Box::new(FixedClock(fixed_now())),
            session_root_override: Some(root.to_path_buf()),
        }
    }

    #[test]
    fn what_all_writes_both_files_and_appends_ttl_events() {
        let tmp = TempDir::new().unwrap();
        let session = session::open_or_create_under(tmp.path(), "s1").unwrap();
        // Stale todo: created 10 days ago, no valid_until → uses class
        // default (7d) → expired at fixed_now().
        let event = make_create(1, 1, "stale", fixed_now() - chrono::Duration::days(10));
        session.append_events(&[event]).unwrap();

        let mut out: Vec<u8> = Vec::new();
        let opts = build_opts(tmp.path(), "s1", What::All);
        run_review(opts, &mut out).unwrap();
        assert!(out.is_empty(), "All-mode should not write to stdout");

        assert!(session.paths.root.join("todos.md").exists());
        assert!(session.paths.root.join("decisions.md").exists());

        // Events log grew by exactly one synthesised expiry line.
        let after = session::read_events(&session.paths.events).unwrap();
        assert_eq!(after.len(), 2, "{after:?}");
    }

    #[test]
    fn what_todos_writes_only_todos_md() {
        let tmp = TempDir::new().unwrap();
        let session = session::open_or_create_under(tmp.path(), "s1").unwrap();
        let event = make_create(1, 1, "active", fixed_now());
        session.append_events(&[event]).unwrap();

        let mut out: Vec<u8> = Vec::new();
        let opts = build_opts(tmp.path(), "s1", What::Todos);
        run_review(opts, &mut out).unwrap();

        assert!(session.paths.root.join("todos.md").exists());
        assert!(!session.paths.root.join("decisions.md").exists());
    }

    #[test]
    fn what_decisions_writes_only_decisions_md() {
        let tmp = TempDir::new().unwrap();
        let session = session::open_or_create_under(tmp.path(), "s1").unwrap();
        // Seed both a todo (so TTL pass has something to expire) and a
        // decision (so decisions.md is non-empty).
        let create_event = make_create(1, 1, "active", fixed_now());
        let decision_event = Event {
            event_id: ulid::Ulid::from_parts(0, 2),
            ts: fixed_now(),
            reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 100)),
            provenance: Provenance {
                transcript_span: None,
                model: None,
                prompt_version: None,
            },
            kind: EventKind::DecisionRecord(crate::voice::events::DecisionRecord {
                decision_id: ulid::Ulid::from_parts(0, 50),
                text: "use ULIDs".into(),
                alternatives: None,
            }),
        };
        session
            .append_events(&[create_event, decision_event])
            .unwrap();

        let mut out: Vec<u8> = Vec::new();
        let opts = build_opts(tmp.path(), "s1", What::Decisions);
        run_review(opts, &mut out).unwrap();

        assert!(!session.paths.root.join("todos.md").exists());
        assert!(session.paths.root.join("decisions.md").exists());
    }

    #[test]
    fn rerunning_review_is_idempotent_for_already_expired_items() {
        let tmp = TempDir::new().unwrap();
        let session = session::open_or_create_under(tmp.path(), "s1").unwrap();
        let event = make_create(1, 1, "stale", fixed_now() - chrono::Duration::days(10));
        session.append_events(&[event]).unwrap();

        let mut buf: Vec<u8> = Vec::new();
        run_review(build_opts(tmp.path(), "s1", What::All), &mut buf).unwrap();
        let after_first = session::read_events(&session.paths.events).unwrap();
        run_review(build_opts(tmp.path(), "s1", What::All), &mut buf).unwrap();
        let after_second = session::read_events(&session.paths.events).unwrap();

        assert_eq!(
            after_first.len(),
            after_second.len(),
            "second review should add no new events"
        );
    }

    #[test]
    fn what_transcript_streams_to_caller_writer() {
        let tmp = TempDir::new().unwrap();
        let session = session::open_or_create_under(tmp.path(), "s1").unwrap();
        // Seed a transcript with one Final.
        let final_evt = crate::voice::TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, 1),
            text: "hello".into(),
            start: std::time::Duration::from_secs(0),
            end: std::time::Duration::from_secs(1),
            confidence: 1.0,
            words: None,
            speaker: None,
            revisable: false,
        };
        let line = serde_json::to_string(&final_evt).unwrap() + "\n";
        std::fs::write(&session.paths.transcript, line).unwrap();

        let mut out: Vec<u8> = Vec::new();
        let opts = build_opts(tmp.path(), "s1", What::Transcript);
        run_review(opts, &mut out).unwrap();
        let body = String::from_utf8(out).unwrap();
        assert!(body.contains("hello"), "got: {body}");
        // Transcript mode must not touch the session dir.
        assert!(!session.paths.root.join("todos.md").exists());
        assert!(!session.paths.root.join("decisions.md").exists());
    }

    #[test]
    fn atomic_write_handles_extensionless_path() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("noext");
        atomic_write(&target, b"hi").unwrap();
        let body = std::fs::read_to_string(&target).unwrap();
        assert_eq!(body, "hi");
    }
}
