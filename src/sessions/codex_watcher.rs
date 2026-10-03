//! The Codex rollout watcher, which supplements the Codex hook feed.
//!
//! An engine-owned background task over
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` (#1909, ADR-0087), the
//! Codex twin of the Claude transcript [`watcher`](super::watcher).
//!
//! Hooks own a Codex session's *state*; this watcher covers what they miss:
//!
//! 1. **Discovery** — a session started before the daemon (or before the hooks
//!    were installed and trusted) still has a rollout file. Its head-of-file
//!    `session_meta` line yields the session's id and `cwd`, so the session lands
//!    on the right worktree row even before a hook fires.
//! 2. **Ends hooks cannot report.** Archiving an IDE or Desktop chat *moves* its
//!    rollout into `$CODEX_HOME/archived_sessions/`, which is outside the watched
//!    tree, so a rollout that vanishes reads as an end. And a Codex process killed
//!    by SIGHUP (closing its terminal tab), SIGTERM or SIGKILL fires no
//!    `SessionEnd`: while a thread is loaded, Codex holds an exclusive `flock` on
//!    `$CODEX_HOME/thread-writer-locks/<session-id>.lock`, and the kernel drops it
//!    when the process dies, however it dies. A lock this watcher has seen held
//!    that is later free (or gone) ends the session, instead of it lingering for
//!    the registry's TTL.
//! 3. **Idle liveness.** An idle Codex session emits nothing, so it would age out
//!    on the TTL while its process lives on. A held lock is re-reported as a
//!    state-preserving heartbeat.
//!
//! What it reads, and what it never does:
//!
//! - Only the **first line** of a rollout (`session_meta`, bounded to
//!   [`MAX_HEAD_BYTES`]), and from it only `id`/`session_id`, `cwd` and `source`;
//!   never a conversation line. Growth is size/mtime only. Nothing is logged.
//! - **Subagent threads are skipped**: their `source` is an object
//!   (`{"subagent": …}`), and their hook events already carry the parent's id.
//! - It never reports a *state*: growth is a heartbeat, not `working`, because a
//!   rollout keeps being written around the turn's `Stop` and would otherwise
//!   hold an idle session at `working`. So a session only this watcher sees reads
//!   `idle`. It can never report an approval either — the rollout records none.
//! - The lock probe is a non-blocking **shared** `flock` taken and dropped at
//!   once: on a session's first sight, then once per scan while it is tracked
//!   and not ended (an ended session is left alone until its rollout grows).
//!   Against a live holder it fails harmlessly; it can only collide with Codex
//!   acquiring the lock for that same thread in the same instant.
//! - On first sight a rollout whose lock is free or gone is recorded but not
//!   announced — a daemon restart must not list sessions that already ended.
//! - A rollout that is *not* recent is no evidence the session is gone: a Codex
//!   left idle at its prompt writes nothing, and one started before hooks were
//!   installed has no hook to announce it either (#2108). So an unread rollout's
//!   lock is probed too, by the thread id its file name ends in, on every scan;
//!   a held lock announces it whatever its mtime. The name is only a prefilter
//!   (a missing lock file is a cheap `ENOENT`): the head, read once the lock is
//!   held, stays the authority for the id, `cwd` and the subagent check.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{is_session_uuid, Agent, ObserveRequest, SessionEvent, SessionsRegistry};

/// How often the lock of a rollout that has not been read is probed (#2108). A
/// Codex home accumulates thousands of old rollouts, so probing each of them on
/// every [`WATCH_INTERVAL`] would cost thousands of syscalls every few seconds for
/// the daemon's whole life; a held lock on an old rollout is found within this
/// instead (or at once, when the rollout is written to).
const UNREAD_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Codex's home directory override, as Codex itself reads it.
const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// How often the watcher rescans. Matches the Claude transcript watcher.
const WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// A rollout must have been modified within this window to be announced when
/// first seen, so a fresh daemon does not flood the registry with every
/// historical session. Matches the registry's session TTL.
const RECENT_ACTIVITY_WINDOW: Duration = Duration::from_secs(300);

/// How often a live session (lock held, or rollout growing) is re-reported, to
/// keep it inside the registry's 5-minute TTL without a report every scan.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// The most of a rollout's head line the watcher will read. `session_meta`
/// embeds Codex's base instructions (~20 KiB today); a head without a newline
/// within this bound is treated as unreadable.
const MAX_HEAD_BYTES: u64 = 1024 * 1024;

/// Where Codex keeps its state: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CODEX_HOME_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".codex"))
}

/// The fields of a rollout's head line the watcher reads. Everything else,
/// including the embedded instructions, is skipped by serde.
#[derive(Debug, Deserialize)]
struct HeadLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<SessionMeta>,
}

/// The `session_meta` payload fields the watcher reads.
#[derive(Debug, Deserialize)]
struct SessionMeta {
    /// The thread id — the hook `session_id`, the rollout filename's id and the
    /// lock filename, for a non-subagent thread.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// `"cli"`, `"vscode"`, `"exec"`, … for a session; an object
    /// (`{"subagent": …}`) for a subagent thread.
    #[serde(default)]
    source: Option<serde_json::Value>,
}

/// What a rollout's head says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Head {
    /// A top-level session.
    Session { id: String, cwd: Option<PathBuf> },
    /// A subagent thread, or a head the watcher cannot read — never reported.
    Skip,
    /// The first line is not complete yet (Codex is still writing it); read
    /// again once the file grows.
    Incomplete,
}

/// Parses a rollout head line. Schema-tolerant: anything but a `session_meta`
/// line with an id is [`Head::Skip`].
fn parse_head(line: &str) -> Head {
    let Ok(head) = serde_json::from_str::<HeadLine>(line) else {
        return Head::Skip;
    };
    let Some(meta) = head.payload.filter(|_| head.kind == "session_meta") else {
        return Head::Skip;
    };
    if meta
        .source
        .as_ref()
        .is_some_and(serde_json::Value::is_object)
    {
        return Head::Skip;
    }
    match meta
        .id
        .or(meta.session_id)
        .filter(|id| !id.trim().is_empty())
    {
        Some(id) => Head::Session { id, cwd: meta.cwd },
        None => Head::Skip,
    }
}

/// Reads and parses the first line of `path`, bounded to [`MAX_HEAD_BYTES`].
fn read_head(path: &Path) -> Head {
    let Ok(file) = std::fs::File::open(path) else {
        return Head::Skip;
    };
    let mut line = String::new();
    let mut reader = BufReader::new(file.take(MAX_HEAD_BYTES));
    match reader.read_line(&mut line) {
        Ok(_) if line.ends_with('\n') => parse_head(&line),
        // Hit the bound without a newline: not a head this watcher will read.
        Ok(n) if n as u64 >= MAX_HEAD_BYTES => Head::Skip,
        Ok(_) => Head::Incomplete,
        Err(_) => Head::Skip,
    }
}

/// The state of a thread's writer lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockState {
    /// Codex holds it: the thread is loaded in a live process.
    Held,
    /// The file exists and nobody holds it: the holder exited or was killed.
    Free,
    /// No lock file in an existing lock directory: the thread is not loaded.
    Absent,
    /// The probe failed for another reason; treated as no information.
    Unknown,
}

/// Probes `<locks>/<id>.lock` with a non-blocking shared `flock`, released at
/// once. See the module docs for why this cannot disturb a live holder.
#[cfg(unix)]
fn probe_lock(locks: &Path, id: &str) -> LockState {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};

    let file = match std::fs::File::open(locks.join(format!("{id}.lock"))) {
        Ok(file) => file,
        // Without the directory this Codex keeps no locks at all, which says
        // nothing about the session.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && locks.is_dir() => {
            return LockState::Absent
        }
        Err(_) => return LockState::Unknown,
    };
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        // Dropping the guard releases the shared lock straight away.
        Ok(_guard) => LockState::Free,
        Err((_, Errno::EWOULDBLOCK)) => LockState::Held,
        Err(_) => LockState::Unknown,
    }
}

#[cfg(not(unix))]
fn probe_lock(_locks: &Path, _id: &str) -> LockState {
    LockState::Unknown
}

/// The state of thread `id`'s writer lock under the resolved Codex home, for the
/// journal replay (#2108): a held lock is proof a live process has the thread
/// loaded, a free or absent one that it is gone. [`LockState::Unknown`] when no
/// Codex home resolves.
pub(crate) fn probe_thread_lock(id: &str) -> LockState {
    probe_thread_lock_in(codex_home().as_deref(), id)
}

/// [`probe_thread_lock`] under an already-resolved Codex home.
fn probe_thread_lock_in(home: Option<&Path>, id: &str) -> LockState {
    match home {
        Some(home) => probe_lock(&home.join("thread-writer-locks"), id),
        None => LockState::Unknown,
    }
}

/// The watcher's record of one rollout file.
#[derive(Debug, Clone)]
enum Tracked {
    /// Seen but not read: it was not recently active when first seen. Its head
    /// is read if it later grows, or when its thread lock is found held.
    /// `probed` is when that lock was last looked at (#2108).
    Unread { size: u64, probed: SystemTime },
    /// A subagent or unreadable rollout; never reported.
    Skipped,
    /// A session rollout.
    Session(SessionTrack),
}

/// Per-session bookkeeping for a [`Tracked::Session`].
#[derive(Debug, Clone)]
struct SessionTrack {
    id: String,
    cwd: Option<PathBuf>,
    size: u64,
    /// Whether this watcher has seen the thread's lock held. Only then does a
    /// free or missing lock mean the process is gone — a Codex without locks,
    /// or a thread that was never loaded, must not read as ended.
    lock_seen: bool,
    /// Whether this watcher ended the session (it stops reporting it until the
    /// rollout grows again, e.g. `codex resume`).
    ended: bool,
    /// When the session was last reported.
    last_report: SystemTime,
}

/// The watcher's state across scans, keyed by rollout path.
type ScanState = HashMap<PathBuf, Tracked>;

/// One thing a scan asks the registry to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Report the session as present, keeping its current state.
    Seen {
        id: String,
        cwd: Option<PathBuf>,
        transcript_path: PathBuf,
    },
    /// End the session.
    End { id: String },
}

impl Action {
    fn apply(self, registry: &SessionsRegistry) {
        match self {
            Self::Seen {
                id,
                cwd,
                transcript_path,
            } => registry.observe(ObserveRequest {
                agent_id: None,
                pid: None,
                agent: Agent::Codex,
                session_id: id,
                cwd,
                transcript_path: Some(transcript_path),
                // Keeps the current state (a new session starts idle), so the
                // watcher never overrides what the hooks reported.
                event: SessionEvent::TranscriptDiscovered,
                repo: None,
                model: None,
            }),
            Self::End { id } => {
                registry.end(&id, Some("codex rollout watcher"), None);
            }
        }
    }
}

/// Whether `modified` is within [`RECENT_ACTIVITY_WINDOW`] of `now` (a future
/// mtime counts as recent).
fn is_recent(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .map_or(true, |elapsed| elapsed <= RECENT_ACTIVITY_WINDOW)
}

/// Every `rollout-*.jsonl` under `root/YYYY/MM/DD/`, with its size and mtime.
fn list_rollouts(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut out = Vec::new();
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    for year in subdirs(root) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                let Ok(files) = std::fs::read_dir(&day) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    let is_rollout = path.extension().is_some_and(|e| e == "jsonl")
                        && path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("rollout-"));
                    if !is_rollout {
                        continue;
                    }
                    let Ok(meta) = file.metadata() else {
                        continue;
                    };
                    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    out.push((path, meta.len(), modified));
                }
            }
        }
    }
    out
}

/// Scans `sessions` (and probes locks under `locks`), updating `state` and
/// returning what the registry should hear. `probe` is injected so tests can
/// script lock states. Pure apart from `state` and the file reads.
fn scan(
    sessions: &Path,
    locks: &Path,
    state: &mut ScanState,
    now: SystemTime,
    probe: &dyn Fn(&Path, &str) -> LockState,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut present = HashSet::new();
    for (path, size, modified) in list_rollouts(sessions) {
        present.insert(path.clone());
        let recent = is_recent(modified, now);
        let tracked = match state.remove(&path) {
            None if !recent => unread_or_held(&path, size, None, locks, now, probe, &mut actions),
            // First sight of a recent rollout, or growth of one not read yet.
            None => first_read(&path, size, locks, now, probe, &mut actions),
            Some(Tracked::Unread { size: old, .. }) if size != old && recent => {
                first_read(&path, size, locks, now, probe, &mut actions)
            }
            Some(Tracked::Unread { probed, .. }) => {
                unread_or_held(&path, size, Some(probed), locks, now, probe, &mut actions)
            }
            Some(Tracked::Skipped) => Tracked::Skipped,
            Some(Tracked::Session(track)) => {
                Tracked::Session(rescan(track, &path, size, locks, now, probe, &mut actions))
            }
        };
        state.insert(path, tracked);
    }
    // A rollout that left the tree was archived (moved to `archived_sessions/`)
    // or deleted: either way the session is over.
    state.retain(|path, tracked| {
        if present.contains(path) {
            return true;
        }
        if let Tracked::Session(track) = tracked {
            if !track.ended {
                actions.push(Action::End {
                    id: track.id.clone(),
                });
            }
        }
        false
    });
    actions
}

/// The thread id a rollout's file name ends in: `rollout-<timestamp>-<id>.jsonl`,
/// where the id is a UUID. `None` for a name that does not end in one.
fn thread_id_from_file_name(path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    let split = stem.len().checked_sub(36)?;
    let id = stem.get(split..)?;
    let separated = split
        .checked_sub(1)
        .is_some_and(|dash| stem.as_bytes().get(dash) == Some(&b'-'));
    (separated && is_session_uuid(id)).then_some(id)
}

/// Handles a rollout that is not recent and has not been read: it stays
/// [`Tracked::Unread`] unless its thread's lock is held, in which case a live
/// process has it loaded and it is read now, whatever its mtime (#2108). `probed`
/// is when the lock was last looked at, if it has been: it is not looked at again
/// before [`UNREAD_PROBE_INTERVAL`] has passed.
fn unread_or_held(
    path: &Path,
    size: u64,
    probed: Option<SystemTime>,
    locks: &Path,
    now: SystemTime,
    probe: &dyn Fn(&Path, &str) -> LockState,
    actions: &mut Vec<Action>,
) -> Tracked {
    if let Some(last) = probed {
        if now.duration_since(last).unwrap_or_default() < UNREAD_PROBE_INTERVAL {
            return Tracked::Unread { size, probed: last };
        }
    }
    match thread_id_from_file_name(path) {
        Some(id) if probe(locks, id) == LockState::Held => {
            first_read(path, size, locks, now, probe, actions)
        }
        _ => Tracked::Unread { size, probed: now },
    }
}

/// Reads a rollout's head for the first time, announcing it when it is a
/// session whose thread is not known to be gone.
fn first_read(
    path: &Path,
    size: u64,
    locks: &Path,
    now: SystemTime,
    probe: &dyn Fn(&Path, &str) -> LockState,
    actions: &mut Vec<Action>,
) -> Tracked {
    let (id, cwd) = match read_head(path) {
        Head::Skip => return Tracked::Skipped,
        Head::Incomplete => return Tracked::Unread { size, probed: now },
        Head::Session { id, cwd } => (id, cwd),
    };
    let lock = probe(locks, &id);
    // A free or missing lock means the thread already ended (the daemon started
    // after it did): record it, so later growth revives it, but do not list it.
    let gone = matches!(lock, LockState::Free | LockState::Absent);
    if !gone {
        actions.push(Action::Seen {
            id: id.clone(),
            cwd: cwd.clone(),
            transcript_path: path.to_path_buf(),
        });
    }
    Tracked::Session(SessionTrack {
        id,
        cwd,
        size,
        lock_seen: lock == LockState::Held,
        ended: gone,
        last_report: now,
    })
}

/// Re-examines a known session: growth revives an ended one, a lock seen held
/// and now released ends it, and a live one is heartbeated.
fn rescan(
    mut track: SessionTrack,
    path: &Path,
    size: u64,
    locks: &Path,
    now: SystemTime,
    probe: &dyn Fn(&Path, &str) -> LockState,
    actions: &mut Vec<Action>,
) -> SessionTrack {
    let grew = size > track.size;
    track.size = size;
    if track.ended {
        if !grew {
            return track;
        }
        // Written to again after we ended it (`codex resume`): start over, and
        // report it this scan rather than a heartbeat interval from now.
        track.ended = false;
        track.lock_seen = false;
        track.last_report = SystemTime::UNIX_EPOCH;
    }
    let lock = probe(locks, &track.id);
    if lock == LockState::Held {
        track.lock_seen = true;
    }
    if track.lock_seen && matches!(lock, LockState::Free | LockState::Absent) {
        track.ended = true;
        actions.push(Action::End {
            id: track.id.clone(),
        });
        return track;
    }
    let alive = grew || lock == LockState::Held;
    let due = now
        .duration_since(track.last_report)
        .is_ok_and(|since| since >= HEARTBEAT_INTERVAL);
    if alive && due {
        track.last_report = now;
        actions.push(Action::Seen {
            id: track.id.clone(),
            cwd: track.cwd.clone(),
            transcript_path: path.to_path_buf(),
        });
    }
    track
}

/// Spawns the watcher loop, returning its [`JoinHandle`].
///
/// Rescans every [`WATCH_INTERVAL`] and applies each action to `registry`,
/// until `token` is cancelled. Parks idle when no Codex home can be resolved.
/// Must be called from within a tokio runtime.
pub fn spawn(registry: Arc<SessionsRegistry>, token: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(home) = codex_home() else {
            tracing::debug!("no Codex home; Codex rollout watcher idle");
            token.cancelled().await;
            return;
        };
        let sessions = home.join("sessions");
        let locks = home.join("thread-writer-locks");
        tracing::debug!("Codex rollout watcher scanning {}", sessions.display());
        let mut state = ScanState::new();
        loop {
            let (scan_sessions, scan_locks) = (sessions.clone(), locks.clone());
            // Directory walks, head reads and lock probes are blocking I/O.
            let mut owned = std::mem::take(&mut state);
            let (returned, actions) = tokio::task::spawn_blocking(move || {
                let actions = scan(
                    &scan_sessions,
                    &scan_locks,
                    &mut owned,
                    SystemTime::now(),
                    &probe_lock,
                );
                (owned, actions)
            })
            .await
            .unwrap_or_else(|_| (ScanState::new(), Vec::new()));
            state = returned;
            for action in actions {
                action.apply(&registry);
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
    use std::cell::RefCell;
    use std::io::Write;

    use crate::sessions::SessionState;

    const ID: &str = "019a0000-0000-7000-8000-00000000c0de";

    fn head(id: &str, source: serde_json::Value) -> String {
        serde_json::json!({
            "timestamp": "2026-09-25T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "session_id": id,
                "cwd": "/work/repo",
                "originator": "codex-tui",
                "source": source,
                "base_instructions": { "text": "x".repeat(2048) },
            },
        })
        .to_string()
    }

    /// Writes `root/2026/09/25/rollout-…-<id>.jsonl` with `lines`.
    fn write_rollout(root: &Path, id: &str, lines: &[String]) -> PathBuf {
        let dir = root.join("2026").join("09").join("25");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-25T00-00-00-{id}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    fn append(path: &Path, line: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// A scripted lock probe: returns whatever the cell currently holds.
    fn scripted(cell: &RefCell<LockState>) -> impl Fn(&Path, &str) -> LockState + '_ {
        move |_, _| *cell.borrow()
    }

    fn ids(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .map(|a| match a {
                Action::Seen { id, .. } => format!("seen:{id}"),
                Action::End { id } => format!("end:{id}"),
            })
            .collect()
    }

    #[test]
    fn parse_head_reads_a_session_and_skips_subagents_and_junk() {
        assert_eq!(
            parse_head(&head(ID, serde_json::json!("cli"))),
            Head::Session {
                id: ID.to_string(),
                cwd: Some(PathBuf::from("/work/repo")),
            }
        );
        let subagent = serde_json::json!({ "subagent": { "thread_spawn": {} } });
        assert_eq!(parse_head(&head(ID, subagent)), Head::Skip);
        assert_eq!(parse_head("not json"), Head::Skip);
        assert_eq!(
            parse_head(r#"{"type":"response_item","payload":{"id":"x"}}"#),
            Head::Skip
        );
        assert_eq!(
            parse_head(r#"{"type":"session_meta","payload":{"cwd":"/w"}}"#),
            Head::Skip
        );
        // `session_id` stands in when `id` is absent.
        assert_eq!(
            parse_head(r#"{"type":"session_meta","payload":{"session_id":"s"}}"#),
            Head::Session {
                id: "s".to_string(),
                cwd: None
            }
        );
    }

    #[test]
    fn read_head_needs_a_complete_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        std::fs::write(&path, head(ID, serde_json::json!("vscode"))).unwrap();
        // No trailing newline yet: Codex is mid-write, so it is read later.
        assert_eq!(read_head(&path), Head::Incomplete);
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_head(&path), Head::Incomplete);
        std::fs::write(
            &path,
            format!("{}\n{{}}\n", head(ID, serde_json::json!("vscode"))),
        )
        .unwrap();
        assert!(matches!(read_head(&path), Head::Session { .. }));
        assert_eq!(read_head(&tmp.path().join("missing")), Head::Skip);
    }

    #[test]
    fn a_recent_rollout_is_discovered_once_with_its_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let lock = RefCell::new(LockState::Unknown);
        let mut state = ScanState::new();
        let now = SystemTime::now();
        let first = scan(tmp.path(), tmp.path(), &mut state, now, &scripted(&lock));
        assert_eq!(
            first,
            vec![Action::Seen {
                id: ID.to_string(),
                cwd: Some(PathBuf::from("/work/repo")),
                transcript_path: path,
            }]
        );
        // Nothing new, nothing due: silence.
        assert!(scan(tmp.path(), tmp.path(), &mut state, now, &scripted(&lock)).is_empty());
    }

    #[test]
    fn subagent_rollouts_and_non_rollout_files_are_never_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = serde_json::json!({ "subagent": "review" });
        write_rollout(tmp.path(), "child", &[head("child", sub)]);
        let dir = tmp.path().join("2026/09/25");
        std::fs::write(dir.join("notes.jsonl"), "{}\n").unwrap();
        std::fs::write(tmp.path().join("rollout-loose.jsonl"), "{}\n").unwrap();
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let now = SystemTime::now();
        assert!(scan(tmp.path(), tmp.path(), &mut state, now, &scripted(&lock)).is_empty());
        let later = now + HEARTBEAT_INTERVAL * 2;
        assert!(scan(tmp.path(), tmp.path(), &mut state, later, &scripted(&lock)).is_empty());
    }

    #[test]
    fn an_old_rollout_is_recorded_silently_and_read_when_it_grows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let lock = RefCell::new(LockState::Unknown);
        let mut state = ScanState::new();
        // Seen from far in the future, the file is not recent.
        let far = SystemTime::now() + RECENT_ACTIVITY_WINDOW * 10;
        assert!(scan(tmp.path(), tmp.path(), &mut state, far, &scripted(&lock)).is_empty());
        assert!(matches!(state[&path], Tracked::Unread { .. }));
        // Resumed: it grows and is recent again, so its head is read now.
        append(&path, "{}");
        let now = SystemTime::now();
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                now,
                &scripted(&lock)
            )),
            vec![format!("seen:{ID}")]
        );
    }

    #[test]
    fn an_old_rollout_whose_lock_is_held_is_announced_regardless_of_its_age() {
        // #2108: a Codex started before hooks were installed, idle for hours,
        // still holds its thread lock while it lives.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let far = SystemTime::now() + RECENT_ACTIVITY_WINDOW * 10;
        assert_eq!(
            scan(tmp.path(), tmp.path(), &mut state, far, &scripted(&lock)),
            vec![Action::Seen {
                id: ID.to_string(),
                cwd: Some(PathBuf::from("/work/repo")),
                transcript_path: path.clone(),
            }]
        );
        assert!(matches!(state[&path], Tracked::Session(_)));
        // Tracked now: held and not yet due is quiet, and the release ends it.
        assert!(scan(tmp.path(), tmp.path(), &mut state, far, &scripted(&lock)).is_empty());
        *lock.borrow_mut() = LockState::Free;
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                far,
                &scripted(&lock)
            )),
            vec![format!("end:{ID}")]
        );
    }

    #[test]
    fn an_old_rollout_is_announced_when_its_lock_becomes_held_later() {
        // A `codex resume` that has not written yet: still `Unread`, then the
        // lock appears and a later probe announces it without any growth.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let lock = RefCell::new(LockState::Absent);
        let mut state = ScanState::new();
        let far = SystemTime::now() + RECENT_ACTIVITY_WINDOW * 10;
        assert!(scan(tmp.path(), tmp.path(), &mut state, far, &scripted(&lock)).is_empty());
        assert!(matches!(state[&path], Tracked::Unread { .. }));
        *lock.borrow_mut() = LockState::Held;
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                far + UNREAD_PROBE_INTERVAL,
                &scripted(&lock)
            )),
            vec![format!("seen:{ID}")]
        );
    }

    #[test]
    fn unread_rollouts_are_probed_once_per_interval_not_every_scan() {
        // A Codex home holds thousands of old rollouts; probing each on every
        // 5s scan would be thousands of syscalls for the daemon's whole life.
        let tmp = tempfile::tempdir().unwrap();
        write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let probes = std::cell::Cell::new(0_u32);
        let counting = |_: &Path, _: &str| {
            probes.set(probes.get() + 1);
            LockState::Absent
        };
        let mut state = ScanState::new();
        let far = SystemTime::now() + RECENT_ACTIVITY_WINDOW * 10;
        scan(tmp.path(), tmp.path(), &mut state, far, &counting);
        assert_eq!(probes.get(), 1, "probed on first sight");
        for scans in 1..=10 {
            scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                far + WATCH_INTERVAL * scans,
                &counting,
            );
        }
        assert_eq!(probes.get(), 1, "not again within the interval");
        scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            far + UNREAD_PROBE_INTERVAL,
            &counting,
        );
        assert_eq!(probes.get(), 2, "once the interval has passed");
    }

    #[test]
    fn an_old_rollout_with_a_free_or_unknown_lock_stays_unread() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let far = SystemTime::now() + RECENT_ACTIVITY_WINDOW * 10;
        for held in [LockState::Free, LockState::Absent, LockState::Unknown] {
            let lock = RefCell::new(held);
            let mut state = ScanState::new();
            assert!(scan(tmp.path(), tmp.path(), &mut state, far, &scripted(&lock)).is_empty());
            assert!(matches!(state[&path], Tracked::Unread { .. }), "{held:?}");
        }
    }

    #[test]
    fn an_old_subagent_rollout_with_a_held_lock_is_skipped_not_announced() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = serde_json::json!({ "subagent": "review" });
        let path = write_rollout(tmp.path(), ID, &[head(ID, sub)]);
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let far = SystemTime::now() + RECENT_ACTIVITY_WINDOW * 10;
        assert!(scan(tmp.path(), tmp.path(), &mut state, far, &scripted(&lock)).is_empty());
        assert!(matches!(state[&path], Tracked::Skipped));
    }

    #[test]
    fn thread_ids_come_only_from_a_uuid_ending_the_file_name() {
        let named = |name: &str| thread_id_from_file_name(Path::new(name)).map(str::to_string);
        assert_eq!(
            named(&format!("rollout-2026-09-25T00-00-00-{ID}.jsonl")),
            Some(ID.to_string())
        );
        // No UUID, a short name, a non-hex group, or no separating dash.
        assert_eq!(named("rollout-2026-09-25T00-00-00-child.jsonl"), None);
        assert_eq!(named("x.jsonl"), None);
        assert_eq!(
            named("rollout-019a0000-0000-7000-8000-00000000zzzz.jsonl"),
            None
        );
        assert_eq!(named(&format!("rollout{ID}.jsonl")), None);
        assert_eq!(named(&format!("{ID}.jsonl")), None);
    }

    #[test]
    fn a_held_lock_heartbeats_and_its_release_ends_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        assert_eq!(
            scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock)).len(),
            1
        );
        // Held, not yet due: quiet. Held and due: a heartbeat.
        let t1 = t0 + WATCH_INTERVAL;
        assert!(scan(tmp.path(), tmp.path(), &mut state, t1, &scripted(&lock)).is_empty());
        let t2 = t0 + HEARTBEAT_INTERVAL;
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                t2,
                &scripted(&lock)
            )),
            vec![format!("seen:{ID}")]
        );
        // The process was killed: the kernel dropped its lock.
        *lock.borrow_mut() = LockState::Free;
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                t2,
                &scripted(&lock)
            )),
            vec![format!("end:{ID}")]
        );
        // Ended once, not again.
        let t3 = t2 + HEARTBEAT_INTERVAL;
        assert!(scan(tmp.path(), tmp.path(), &mut state, t3, &scripted(&lock)).is_empty());
    }

    #[test]
    fn a_lock_never_seen_held_does_not_end_the_session() {
        // An older Codex without a lock directory probes `Unknown`.
        let tmp = tempfile::tempdir().unwrap();
        write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let lock = RefCell::new(LockState::Unknown);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        let later = t0 + HEARTBEAT_INTERVAL * 3;
        assert!(scan(tmp.path(), tmp.path(), &mut state, later, &scripted(&lock)).is_empty());
        // An unknown probe after a held one is not an end either.
        *lock.borrow_mut() = LockState::Held;
        scan(tmp.path(), tmp.path(), &mut state, later, &scripted(&lock));
        *lock.borrow_mut() = LockState::Unknown;
        let actions = scan(tmp.path(), tmp.path(), &mut state, later, &scripted(&lock));
        assert!(!ids(&actions).iter().any(|a| a.starts_with("end:")));
    }

    #[test]
    fn growth_without_a_lock_is_a_heartbeat_not_a_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("exec"))]);
        let lock = RefCell::new(LockState::Unknown);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        append(&path, "{}");
        let due = t0 + HEARTBEAT_INTERVAL;
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                due,
                &scripted(&lock)
            )),
            vec![format!("seen:{ID}")]
        );
    }

    #[test]
    fn an_archived_rollout_ends_its_session_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("vscode"))]);
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let now = SystemTime::now();
        scan(tmp.path(), tmp.path(), &mut state, now, &scripted(&lock));
        // Archiving moves the file out of the watched tree.
        let archived = tmp.path().join("archived.jsonl");
        std::fs::rename(&path, archived).unwrap();
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                now,
                &scripted(&lock)
            )),
            vec![format!("end:{ID}")]
        );
        assert!(state.is_empty());
        assert!(scan(tmp.path(), tmp.path(), &mut state, now, &scripted(&lock)).is_empty());
    }

    #[test]
    fn a_resumed_session_is_reported_again_after_an_end() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        *lock.borrow_mut() = LockState::Absent;
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        // `codex resume <id>` appends to the same rollout under the same id.
        *lock.borrow_mut() = LockState::Held;
        append(&path, "{}");
        let due = t0 + HEARTBEAT_INTERVAL;
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                due,
                &scripted(&lock)
            )),
            vec![format!("seen:{ID}")]
        );
    }

    #[test]
    fn a_head_still_being_written_is_read_once_it_is_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("2026/09/25");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-25T00-00-00-{ID}.jsonl"));
        let full = head(ID, serde_json::json!("cli"));
        std::fs::write(&path, &full[..full.len() / 2]).unwrap();
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let now = SystemTime::now();
        assert!(scan(tmp.path(), tmp.path(), &mut state, now, &scripted(&lock)).is_empty());
        assert!(matches!(state[&path], Tracked::Unread { .. }));
        std::fs::write(&path, format!("{full}\n")).unwrap();
        assert_eq!(
            ids(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                now,
                &scripted(&lock)
            )),
            vec![format!("seen:{ID}")]
        );
    }

    #[test]
    fn a_restart_does_not_list_a_session_whose_lock_is_already_released() {
        for gone in [LockState::Free, LockState::Absent] {
            let tmp = tempfile::tempdir().unwrap();
            let path = write_rollout(tmp.path(), ID, &[head(ID, serde_json::json!("cli"))]);
            let lock = RefCell::new(gone);
            let mut state = ScanState::new();
            let now = SystemTime::now();
            assert!(
                scan(tmp.path(), tmp.path(), &mut state, now, &scripted(&lock)).is_empty(),
                "{gone:?}"
            );
            // Not listed, not ended: nothing to say until it is written to again.
            let later = now + HEARTBEAT_INTERVAL * 2;
            assert!(scan(tmp.path(), tmp.path(), &mut state, later, &scripted(&lock)).is_empty());
            // `codex resume` reloads it and appends: reported straight away.
            *lock.borrow_mut() = LockState::Held;
            append(&path, "{}");
            assert_eq!(
                ids(&scan(
                    tmp.path(),
                    tmp.path(),
                    &mut state,
                    later,
                    &scripted(&lock)
                )),
                vec![format!("seen:{ID}")]
            );
        }
    }

    #[test]
    fn scanning_a_missing_root_is_empty() {
        let lock = RefCell::new(LockState::Unknown);
        let mut state = ScanState::new();
        let missing = Path::new("/nonexistent/codex/sessions");
        assert!(scan(
            missing,
            missing,
            &mut state,
            SystemTime::now(),
            &scripted(&lock)
        )
        .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn probe_lock_tells_held_from_free_and_absent() {
        use nix::fcntl::{Flock, FlockArg};

        let tmp = tempfile::tempdir().unwrap();
        // No lock directory at all: a Codex without locks, so no information.
        assert_eq!(
            probe_lock(&tmp.path().join("missing"), ID),
            LockState::Unknown
        );
        assert_eq!(probe_lock(tmp.path(), ID), LockState::Absent);
        let path = tmp.path().join(format!("{ID}.lock"));
        std::fs::write(&path, "").unwrap();
        assert_eq!(probe_lock(tmp.path(), ID), LockState::Free);
        // Codex holds its lock exclusively, from another open file description.
        let holder = Flock::lock(
            std::fs::File::open(&path).unwrap(),
            FlockArg::LockExclusiveNonblock,
        )
        .unwrap();
        assert_eq!(probe_lock(tmp.path(), ID), LockState::Held);
        // The probe did not disturb the holder: it still holds the lock.
        assert_eq!(probe_lock(tmp.path(), ID), LockState::Held);
        drop(holder);
        assert_eq!(probe_lock(tmp.path(), ID), LockState::Free);
    }

    #[test]
    fn the_thread_lock_probe_reads_the_resolved_homes_lock_directory() {
        let tmp = tempfile::tempdir().unwrap();
        // No home resolves at all: no information.
        assert_eq!(probe_thread_lock_in(None, ID), LockState::Unknown);
        // A home without a lock directory is a Codex that keeps no locks.
        assert_eq!(
            probe_thread_lock_in(Some(tmp.path()), ID),
            LockState::Unknown
        );
        let locks = tmp.path().join("thread-writer-locks");
        std::fs::create_dir(&locks).unwrap();
        assert_eq!(
            probe_thread_lock_in(Some(tmp.path()), ID),
            LockState::Absent
        );
        std::fs::write(locks.join(format!("{ID}.lock")), "").unwrap();
        assert_eq!(probe_thread_lock_in(Some(tmp.path()), ID), LockState::Free);
    }

    #[test]
    fn actions_apply_to_the_registry_as_codex_sessions() {
        let registry = SessionsRegistry::new();
        Action::Seen {
            id: ID.to_string(),
            cwd: Some(PathBuf::from("/work/repo")),
            transcript_path: PathBuf::from("/r.jsonl"),
        }
        .apply(&registry);
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].agent, Agent::Codex);
        assert_eq!(listed[0].state, SessionState::Idle);
        assert_eq!(listed[0].cwd.as_deref(), Some(Path::new("/work/repo")));
        Action::End { id: ID.to_string() }.apply(&registry);
        assert_eq!(registry.list()[0].state, SessionState::Ended);
    }
}
