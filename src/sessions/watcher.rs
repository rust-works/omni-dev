//! The transcript watcher (Feed 2): an engine-owned background task that scans
//! `~/.claude/projects/**/*.jsonl` for new and growing session transcripts and
//! feeds the [`SessionsRegistry`].
//!
//! It exists to cover the two gaps a purely hook-driven feed leaves:
//!
//! 1. **Discovery** — a session that started before the daemon (or before hooks
//!    were installed) fires no `SessionStart` the daemon can see; its transcript
//!    file still appears, so the watcher discovers it.
//! 2. **The thinking window** — between `UserPromptSubmit` and the first
//!    `PreToolUse` (~5–15s) no hook fires, but the transcript keeps growing, so
//!    the watcher marks the session `working` through the gap.
//!
//! Per ADR-0052 the watcher parses **only file presence and growth (size/mtime)**
//! — never the per-line transcript schema, which is explicitly internal and
//! version-unstable. The `session_id` comes from the filename stem; `cwd` is left
//! unknown (the encoded directory name is a lossy `/`→`-` transform that cannot be
//! reliably reversed — the hook feed supplies the real `cwd`, and
//! [`SessionsRegistry::observe`] never clobbers a known `cwd` with `None`).
//!
//! Only transcripts touched within [`RECENT_ACTIVITY_WINDOW`] are surfaced, so a
//! fresh daemon does not flood the registry with hundreds of long-dead historical
//! sessions on its first scan — ancient files are recorded (for later
//! growth comparison) but never announced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{ObserveRequest, SessionEvent, SessionsRegistry};

/// Environment override for the Claude config directory, mirroring Claude Code's
/// own `CLAUDE_CONFIG_DIR`. When unset the watcher uses `~/.claude`.
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Direct override for the transcripts root, used by tests (and as an escape
/// hatch) to point the watcher at an arbitrary directory.
const PROJECTS_DIR_ENV: &str = "OMNI_DEV_CLAUDE_PROJECTS_DIR";

/// How often the watcher rescans the transcripts tree. Short enough to catch the
/// thinking window (~5–15s) without stat-ing the tree wastefully.
const WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// A transcript must have been modified within this window to be surfaced as a
/// discovery/growth sighting. Matches the registry's session TTL: a file touched
/// this recently corresponds to a session the registry would still hold live.
/// Older files are recorded silently so a later resume still registers as growth.
const RECENT_ACTIVITY_WINDOW: Duration = Duration::from_secs(300);

/// The last observed size of a transcript file, keyed by path — the watcher's
/// only persistent state across scans, used to detect growth.
type ScanState = HashMap<PathBuf, u64>;

/// A single sighting produced by a scan: which session, its transcript path, and
/// whether it was newly discovered or seen to grow.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sighting {
    session_id: String,
    transcript_path: PathBuf,
    event: SessionEvent,
}

impl Sighting {
    /// Converts the sighting into the registry ingest request. `cwd`/`repo`/
    /// `model` are unknown to the watcher and left for the hook feed to fill.
    fn into_observe(self) -> ObserveRequest {
        ObserveRequest {
            session_id: self.session_id,
            cwd: None,
            transcript_path: Some(self.transcript_path),
            event: self.event,
            repo: None,
            model: None,
        }
    }
}

/// The transcripts root: `$OMNI_DEV_CLAUDE_PROJECTS_DIR` (test/escape override),
/// else `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`. `None` only
/// when no home directory can be resolved and no override is set.
fn projects_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(PROJECTS_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os(CLAUDE_CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir).join("projects"));
    }
    dirs::home_dir().map(|home| home.join(".claude").join("projects"))
}

/// Whether `modified` is within [`RECENT_ACTIVITY_WINDOW`] of `now`. A file whose
/// mtime is in the future (clock skew) counts as recent.
fn is_recent(modified: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(modified) {
        Ok(elapsed) => elapsed <= RECENT_ACTIVITY_WINDOW,
        Err(_) => true,
    }
}

/// Scans `root` for `*.jsonl` transcripts and returns the sightings since the
/// previous scan, updating `state` (path → last size) in place.
///
/// The layout is `root/<encoded-cwd>/<session-id>.jsonl`; the watcher walks the
/// two levels with `read_dir` (no external walker, no new dependency). A file is
/// surfaced only when it was modified within [`RECENT_ACTIVITY_WINDOW`]:
///
/// - unseen path → [`SessionEvent::TranscriptDiscovered`] (and its size recorded);
/// - larger than last seen → [`SessionEvent::TranscriptGrew`];
/// - unchanged → nothing.
///
/// An older-than-window file has its size recorded but is not surfaced, so the
/// first scan of a long-lived `~/.claude/projects` does not announce every
/// historical session. Pure and side-effect-free apart from `state`, so it is
/// unit-tested against a temp directory.
fn scan(root: &Path, state: &mut ScanState, now: SystemTime) -> Vec<Sighting> {
    let mut sightings = Vec::new();
    let Ok(project_dirs) = std::fs::read_dir(root) else {
        return sightings;
    };
    for project in project_dirs.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(session_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let Ok(meta) = file.metadata() else {
                continue;
            };
            let size = meta.len();
            let recent = meta.modified().is_ok_and(|m| is_recent(m, now));
            let previous = state.insert(path.clone(), size);
            if !recent {
                // Record the size (for a future growth comparison) but do not
                // announce an inactive session.
                continue;
            }
            let event = match previous {
                None => SessionEvent::TranscriptDiscovered,
                Some(prev) if size > prev => SessionEvent::TranscriptGrew,
                Some(_) => continue,
            };
            sightings.push(Sighting {
                session_id,
                transcript_path: path,
                event,
            });
        }
    }
    sightings
}

/// Spawns the watcher loop, returning its [`JoinHandle`].
///
/// The loop rescans every [`WATCH_INTERVAL`] and feeds every sighting to
/// `registry`, until `token` is cancelled. A no-op loop when no transcripts root
/// can be resolved (it parks on the cancel token). Must be called from within a
/// tokio runtime.
pub fn spawn(registry: Arc<SessionsRegistry>, token: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(root) = projects_dir() else {
            tracing::debug!("no Claude projects dir; sessions transcript watcher idle");
            token.cancelled().await;
            return;
        };
        tracing::debug!("sessions transcript watcher scanning {}", root.display());
        let mut state = ScanState::new();
        loop {
            let scan_root = root.clone();
            // File stat is blocking disk I/O, so run each scan on a blocking
            // thread; the state map moves in and back out so it persists across
            // scans without a lock.
            let mut owned_state = std::mem::take(&mut state);
            let (returned_state, sightings) = tokio::task::spawn_blocking(move || {
                let sightings = scan(&scan_root, &mut owned_state, SystemTime::now());
                (owned_state, sightings)
            })
            .await
            .unwrap_or_else(|_| (ScanState::new(), Vec::new()));
            state = returned_state;
            for sighting in sightings {
                registry.observe(sighting.into_observe());
            }
            tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(WATCH_INTERVAL) => {}
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Creates `root/<project>/<session>.jsonl` with `contents`, returning its path.
    fn write_transcript(root: &Path, project: &str, session: &str, contents: &[u8]) -> PathBuf {
        let dir = root.join(project);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        path
    }

    #[test]
    fn scan_discovers_then_detects_growth() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = SystemTime::now();
        write_transcript(root, "-home-me-proj", "sess-1", b"line one\n");

        let mut state = ScanState::new();
        // First scan: the recent file is discovered.
        let first = scan(root, &mut state, now);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].session_id, "sess-1");
        assert_eq!(first[0].event, SessionEvent::TranscriptDiscovered);

        // A second scan with no change surfaces nothing.
        assert!(scan(root, &mut state, now).is_empty());

        // Growth is detected.
        write_transcript(root, "-home-me-proj", "sess-1", b"line one\nline two\n");
        let grew = scan(root, &mut state, now);
        assert_eq!(grew.len(), 1);
        assert_eq!(grew[0].event, SessionEvent::TranscriptGrew);
    }

    #[test]
    fn scan_ignores_non_jsonl_and_empty_stems() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = SystemTime::now();
        // A non-jsonl file and a dotfile with an empty stem are both skipped.
        write_transcript(root, "proj", "notes", b"x"); // notes.jsonl — valid
        std::fs::write(root.join("proj").join("readme.txt"), b"y").unwrap();
        std::fs::write(root.join("proj").join(".jsonl"), b"z").unwrap();

        let mut state = ScanState::new();
        let sightings = scan(root, &mut state, now);
        let ids: Vec<&str> = sightings.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["notes"]);
    }

    #[test]
    fn scan_does_not_announce_old_transcripts_but_records_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let path = write_transcript(root, "proj", "ancient", b"old\n");
        // Pretend "now" is far in the future, so the file is outside the window.
        let future = SystemTime::now() + Duration::from_secs(100_000);

        let mut state = ScanState::new();
        // Not surfaced (the file looks old relative to the far-future "now")...
        assert!(scan(root, &mut state, future).is_empty());
        // ...but its size was recorded, so a later real growth is caught.
        assert_eq!(state.get(&path).copied(), Some(4));
        // The resume rewrites the file, giving it a fresh (real) mtime, so a scan
        // at real time surfaces the growth even though the file predated the daemon.
        std::fs::write(&path, b"old\nresumed\n").unwrap();
        let grew = scan(root, &mut state, SystemTime::now());
        assert_eq!(grew.len(), 1);
        assert_eq!(grew[0].event, SessionEvent::TranscriptGrew);
    }

    #[test]
    fn scan_of_missing_root_is_empty() {
        let mut state = ScanState::new();
        let sightings = scan(
            Path::new("/no/such/dir/omni-dev-test"),
            &mut state,
            SystemTime::now(),
        );
        assert!(sightings.is_empty());
    }

    #[test]
    fn is_recent_window_boundaries() {
        let now = SystemTime::now();
        assert!(is_recent(now, now));
        assert!(is_recent(now - Duration::from_secs(10), now));
        assert!(!is_recent(now - Duration::from_secs(10_000), now));
        // A future mtime (clock skew) counts as recent.
        assert!(is_recent(now + Duration::from_secs(60), now));
    }

    #[test]
    fn projects_dir_prefers_explicit_override() {
        // The direct override wins regardless of other env; restore afterwards.
        let prev = std::env::var_os(PROJECTS_DIR_ENV);
        std::env::set_var(PROJECTS_DIR_ENV, "/tmp/omni-dev-transcripts");
        assert_eq!(
            projects_dir(),
            Some(PathBuf::from("/tmp/omni-dev-transcripts"))
        );
        match prev {
            Some(v) => std::env::set_var(PROJECTS_DIR_ENV, v),
            None => std::env::remove_var(PROJECTS_DIR_ENV),
        }
    }

    #[tokio::test]
    async fn spawned_watcher_feeds_the_registry_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        write_transcript(tmp.path(), "proj", "sess-live", b"hi\n");
        // Point the watcher at the temp tree.
        std::env::set_var(PROJECTS_DIR_ENV, tmp.path());

        let registry = Arc::new(SessionsRegistry::new());
        let token = CancellationToken::new();
        let handle = spawn(registry.clone(), token.clone());

        // Poll until the first scan lands the session (the loop scans immediately).
        let mut found = false;
        for _ in 0..50 {
            if registry.list().iter().any(|s| s.session_id == "sess-live") {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        token.cancel();
        let _ = handle.await;
        std::env::remove_var(PROJECTS_DIR_ENV);
        assert!(found, "watcher should have discovered the transcript");
    }

    #[test]
    fn scan_skips_loose_files_at_the_root() {
        // A non-directory entry directly under the root (not a project dir) is
        // skipped — `read_dir` on it fails and the scan continues.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("loose.txt"), b"not a project dir").unwrap();
        write_transcript(root, "proj", "sess-1", b"line\n");

        let mut state = ScanState::new();
        let sightings = scan(root, &mut state, SystemTime::now());
        let ids: Vec<&str> = sightings.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["sess-1"], "the loose file must not surface");
    }
}
