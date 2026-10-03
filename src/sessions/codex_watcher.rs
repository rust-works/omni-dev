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
//! 4. **A hook-less session's turn state** (#2135). A session with no hook feed —
//!    started before the hooks were installed and trusted — would otherwise read
//!    `idle` for good. While its thread lock is held, the rollout's last turn
//!    marker says whether a turn is running (below).
//!
//! What it reads, and what it never does:
//!
//! - The **first line** of a rollout (`session_meta`, bounded to
//!   [`MAX_HEAD_BYTES`]), and from it only `id`/`session_id`, `cwd` and `source`.
//! - A **bounded tail** (starting at [`TURN_TAIL_START`], widened to at most
//!   [`TURN_TAIL_MAX`]), for the turn marker closest to the end. That is the
//!   version-sensitive part of the file, so the read is schema-tolerant: a line
//!   counts only if it parses as an `event_msg` whose `payload.type` is
//!   `task_started`, `task_complete` or `turn_aborted`, and anything else — a
//!   truncated or unknown tail, a line that merely quotes a marker name — is
//!   ignored and changes nothing. Only an [`RolloutTurn`] leaves the read; no
//!   conversation content is kept or logged.
//! - Growth is otherwise size/mtime only. Nothing is logged.
//! - **Subagent threads are skipped**: their `source` is an object
//!   (`{"subagent": …}`), and their hook events already carry the parent's id.
//! - Growth is a heartbeat, not `working`: a rollout keeps being written around
//!   the turn's `Stop`, so it would hold an idle session at `working`. The state
//!   comes from the **last turn marker** alone, never from counts or growth, and
//!   only while the thread lock is *held* — a free lock means the session is gone
//!   whatever the marker says, and a Codex without locks makes no claim.
//! - A marker is reported on a **change**, not every scan. A level re-asserted
//!   each scan would race a hook (`UserPromptSubmit` lands before Codex flushes
//!   `task_started`, and the still-closed marker would read it back to `idle`).
//!   The first read of a session claims `working` for an open turn, but never
//!   `idle` for a closed one, so it cannot override what a hook reported. An open
//!   marker in a rollout that has not been written to lately is not claimed, and
//!   neither is the one a session was ended in: a crash leaves a turn open for
//!   good, and `codex resume` of it holds the lock again. Markers are told apart
//!   by the offset of their line, so only a *newer* `task_started` counts.
//! - A read that fails is retried on the next scan, not remembered as done.
//! - The report is passive ([`SessionEvent::RolloutTurn`]): a `waiting_for_*`
//!   state a hook reported is held, an open turn does not start `working` over
//!   an `idle` a hook or the wrapper reported, and the wrapper's `StreamState`,
//!   re-asserted on every poll, has the last word. It can never report an
//!   approval either — the rollout records none.
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
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{is_session_uuid, Agent, ObserveRequest, RolloutTurn, SessionEvent, SessionsRegistry};

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

/// How much of a rollout's end is read first when looking for its last turn
/// marker (#2135). A turn's markers sit among the conversation's own lines, so a
/// busy turn pushes `task_started` far from the end; most rollouts hold theirs
/// well inside this.
const TURN_TAIL_START: u64 = 256 * 1024;

/// The most of a rollout's end read to find a turn marker. A single line of tool
/// output can run to several MiB, so a window inside one holds no marker at all;
/// the window is widened [`TURN_TAIL_GROWTH`]-fold up to this before the marker
/// is called unknown, which changes nothing.
const TURN_TAIL_MAX: u64 = 4 * 1024 * 1024;

/// How much wider the tail window gets each time it holds no marker.
const TURN_TAIL_GROWTH: u64 = 4;

/// The `event_msg` payload types that mark a turn's edges. A line is parsed only
/// if it contains one of these, so nearly every line of a rollout is skipped
/// without being decoded.
const TURN_MARKER_NAMES: [&str; 3] = ["task_started", "task_complete", "turn_aborted"];

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

/// The fields of a rollout line the turn reader looks at. Everything else,
/// including the conversation itself, is skipped by serde.
#[derive(Debug, Deserialize)]
struct EventLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<EventPayload>,
}

/// The one `event_msg` payload field that names the event.
#[derive(Debug, Deserialize)]
struct EventPayload {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

/// What one rollout line says about a turn: `Some` only for an `event_msg`
/// whose `payload.type` is a turn marker. A line that merely mentions a marker
/// name (a `response_item` quoting one, say), or that does not parse, is `None`.
fn parse_turn_marker(line: &str) -> Option<RolloutTurn> {
    if !TURN_MARKER_NAMES.iter().any(|name| line.contains(name)) {
        return None;
    }
    let event = serde_json::from_str::<EventLine>(line).ok()?;
    if event.kind != "event_msg" {
        return None;
    }
    match event.payload?.kind?.as_str() {
        "task_started" => Some(RolloutTurn::Open),
        "task_complete" | "turn_aborted" => Some(RolloutTurn::Closed),
        _ => None,
    }
}

/// A turn marker found in a rollout, and where its line begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marker {
    turn: RolloutTurn,
    /// The byte offset of the marker's line from the start of the file. A rollout
    /// is only ever appended to, so this names one marker for good: it tells a
    /// marker already seen from a newer one that says the same thing.
    at: u64,
}

/// The last turn marker in `window`, a stretch of a rollout ending at its end,
/// with the offset of its line within `window`. `starts_mid_line` is whether the
/// stretch begins past the start of the file, in which case its first line is a
/// fragment and is dropped. A final line Codex has not finished writing fails to
/// parse and is passed over, so the marker before it stands until the next scan.
fn last_turn_marker(window: &[u8], starts_mid_line: bool) -> Option<(RolloutTurn, usize)> {
    let first = if starts_mid_line {
        window.iter().position(|byte| *byte == b'\n')? + 1
    } else {
        0
    };
    let mut end = window.len();
    loop {
        let begin = window
            .get(first..end)?
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(first, |newline| first + newline + 1);
        let line = window.get(begin..end)?;
        if let Some(turn) = std::str::from_utf8(line).ok().and_then(parse_turn_marker) {
            return Some((turn, begin));
        }
        if begin == first {
            return None;
        }
        end = begin - 1;
    }
}

/// Reads the turn marker closest to the end of the rollout at `path`: the last
/// [`TURN_TAIL_START`] bytes, widened [`TURN_TAIL_GROWTH`]-fold up to
/// [`TURN_TAIL_MAX`] while they hold none. `Ok(None)` when no marker lies within
/// that reach, which callers treat as "no information"; an `Err` is a read that
/// failed, which they retry.
fn read_last_turn(path: &Path) -> std::io::Result<Option<Marker>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut window = TURN_TAIL_START;
    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        (&mut file).take(len - start).read_to_end(&mut bytes)?;
        if let Some((turn, begin)) = last_turn_marker(&bytes, start > 0) {
            return Ok(Some(Marker {
                turn,
                at: start + begin as u64,
            }));
        }
        if start == 0 || window >= TURN_TAIL_MAX {
            return Ok(None);
        }
        window = window.saturating_mul(TURN_TAIL_GROWTH).min(TURN_TAIL_MAX);
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
    /// The turn marker last established from the rollout's tail while the lock
    /// was held, if any (#2135). A change from it is what gets reported.
    turn: Option<Marker>,
    /// The rollout size the tail was last read at, so an idle session whose
    /// rollout has not changed costs no read. Unset after a failed read, which is
    /// retried on the next scan.
    turn_read_at: Option<u64>,
    /// The offset of an open marker that must not be claimed as a running turn:
    /// one declined for being old, or the one a session was ended in.
    stale_open: Option<u64>,
}

impl SessionTrack {
    /// Prepares a session that was ended and written to again (`codex resume`) to
    /// be established afresh. A turn it died in stays open in the rollout until
    /// the next one starts, so that marker is remembered as stale rather than
    /// claimed as a turn the new process is running.
    fn forget_turn(&mut self) {
        if let Some(Marker {
            turn: RolloutTurn::Open,
            at,
        }) = self.turn
        {
            self.stale_open = Some(at);
        }
        self.turn = None;
        self.turn_read_at = None;
    }

    /// Reads the rollout's last turn marker when `lock` is held and the rollout
    /// has changed since it was last read, returning the turn to report, if any
    /// (#2135).
    ///
    /// The first marker established is handled with care, because a hook may
    /// already have said more than the rollout can:
    ///
    /// - a closed turn is recorded but not reported, so it can never override a
    ///   state a hook reported (a new session still defaults to `idle`);
    /// - an open turn is reported only if the rollout was written to recently,
    ///   and never if it is the marker already declined or the one the session
    ///   ended in. A crash leaves a turn open for good, and `codex resume` of that
    ///   holds the lock again, so an old open marker is no evidence a turn is
    ///   running; only a newer `task_started` is.
    ///
    /// After that only a change is reported.
    fn refresh_turn(
        &mut self,
        rollout: Rollout<'_>,
        lock: LockState,
        now: SystemTime,
    ) -> Option<RolloutTurn> {
        if lock != LockState::Held || self.turn_read_at == Some(rollout.size) {
            return None;
        }
        // A failed read is retried on the next scan; remembering the size would
        // leave a turn that just ended unread until the rollout grew again.
        let found = read_last_turn(rollout.path).ok()?;
        self.turn_read_at = Some(rollout.size);
        let marker = found?;
        match (self.turn, marker.turn) {
            (None, RolloutTurn::Closed) => {
                self.turn = Some(marker);
                self.stale_open = None;
                None
            }
            (None, RolloutTurn::Open) if self.stale_open == Some(marker.at) => None,
            (None, RolloutTurn::Open) if !is_recent(rollout.modified, now) => {
                self.stale_open = Some(marker.at);
                None
            }
            // The same state again, possibly a newer marker for it: keep the
            // latest offset, report nothing.
            (Some(known), turn) if known.turn == turn => {
                self.turn = Some(marker);
                None
            }
            _ => {
                self.turn = Some(marker);
                self.stale_open = None;
                Some(marker.turn)
            }
        }
    }
}

/// A rollout file as a scan found it.
#[derive(Debug, Clone, Copy)]
struct Rollout<'a> {
    path: &'a Path,
    size: u64,
    modified: SystemTime,
}

/// The watcher's state across scans, keyed by rollout path.
type ScanState = HashMap<PathBuf, Tracked>;

/// One thing a scan asks the registry to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Report the session as present: keeping its current state, unless `turn`
    /// carries a change in the rollout's last turn marker (#2135).
    Seen {
        id: String,
        cwd: Option<PathBuf>,
        transcript_path: PathBuf,
        turn: Option<RolloutTurn>,
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
                turn,
            } => registry.observe(ObserveRequest {
                agent_id: None,
                pid: None,
                agent: Agent::Codex,
                session_id: id,
                cwd,
                transcript_path: Some(transcript_path),
                // Keeps the current state (a new session starts idle) unless a
                // turn marker changed, and even that is passive evidence, so
                // the watcher never overrides what the hooks reported.
                event: turn.map_or(
                    SessionEvent::TranscriptDiscovered,
                    SessionEvent::RolloutTurn,
                ),
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
        let rollout = Rollout {
            path: &path,
            size,
            modified,
        };
        let tracked = match state.remove(&path) {
            None if !recent => unread_or_held(rollout, None, locks, now, probe, &mut actions),
            // First sight of a recent rollout, or growth of one not read yet.
            None => first_read(rollout, locks, now, probe, &mut actions),
            Some(Tracked::Unread { size: old, .. }) if size != old && recent => {
                first_read(rollout, locks, now, probe, &mut actions)
            }
            Some(Tracked::Unread { probed, .. }) => {
                unread_or_held(rollout, Some(probed), locks, now, probe, &mut actions)
            }
            Some(Tracked::Skipped) => Tracked::Skipped,
            Some(Tracked::Session(track)) => {
                Tracked::Session(rescan(track, rollout, locks, now, probe, &mut actions))
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
    rollout: Rollout<'_>,
    probed: Option<SystemTime>,
    locks: &Path,
    now: SystemTime,
    probe: &dyn Fn(&Path, &str) -> LockState,
    actions: &mut Vec<Action>,
) -> Tracked {
    let size = rollout.size;
    if let Some(last) = probed {
        if now.duration_since(last).unwrap_or_default() < UNREAD_PROBE_INTERVAL {
            return Tracked::Unread { size, probed: last };
        }
    }
    match thread_id_from_file_name(rollout.path) {
        Some(id) if probe(locks, id) == LockState::Held => {
            first_read(rollout, locks, now, probe, actions)
        }
        _ => Tracked::Unread { size, probed: now },
    }
}

/// Reads a rollout's head for the first time, announcing it when it is a
/// session whose thread is not known to be gone. A thread whose lock is held also
/// has its last turn marker read (#2135).
fn first_read(
    rollout: Rollout<'_>,
    locks: &Path,
    now: SystemTime,
    probe: &dyn Fn(&Path, &str) -> LockState,
    actions: &mut Vec<Action>,
) -> Tracked {
    let (id, cwd) = match read_head(rollout.path) {
        Head::Skip => return Tracked::Skipped,
        Head::Incomplete => {
            return Tracked::Unread {
                size: rollout.size,
                probed: now,
            }
        }
        Head::Session { id, cwd } => (id, cwd),
    };
    let lock = probe(locks, &id);
    // A free or missing lock means the thread already ended (the daemon started
    // after it did): record it, so later growth revives it, but do not list it.
    let gone = matches!(lock, LockState::Free | LockState::Absent);
    let mut track = SessionTrack {
        id,
        cwd,
        size: rollout.size,
        lock_seen: lock == LockState::Held,
        ended: gone,
        last_report: now,
        turn: None,
        turn_read_at: None,
        stale_open: None,
    };
    if !gone {
        let turn = track.refresh_turn(rollout, lock, now);
        actions.push(Action::Seen {
            id: track.id.clone(),
            cwd: track.cwd.clone(),
            transcript_path: rollout.path.to_path_buf(),
            turn,
        });
    }
    Tracked::Session(track)
}

/// Re-examines a known session: growth revives an ended one, a lock seen held
/// and now released ends it, a change in the rollout's last turn marker is
/// reported at once, and a live one is heartbeated.
fn rescan(
    mut track: SessionTrack,
    rollout: Rollout<'_>,
    locks: &Path,
    now: SystemTime,
    probe: &dyn Fn(&Path, &str) -> LockState,
    actions: &mut Vec<Action>,
) -> SessionTrack {
    let grew = rollout.size > track.size;
    track.size = rollout.size;
    if track.ended {
        if !grew {
            return track;
        }
        // Written to again after we ended it (`codex resume`): start over, and
        // report it this scan rather than a heartbeat interval from now.
        track.ended = false;
        track.lock_seen = false;
        track.last_report = SystemTime::UNIX_EPOCH;
        track.forget_turn();
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
    let turn = track.refresh_turn(rollout, lock, now);
    let alive = grew || lock == LockState::Held;
    let due = now
        .duration_since(track.last_report)
        .is_ok_and(|since| since >= HEARTBEAT_INTERVAL);
    // A turn change is reported when it is read, not a heartbeat interval later,
    // and it counts as the heartbeat.
    if turn.is_some() || (alive && due) {
        track.last_report = now;
        actions.push(Action::Seen {
            id: track.id.clone(),
            cwd: track.cwd.clone(),
            transcript_path: rollout.path.to_path_buf(),
            turn,
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
                turn: None,
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
                turn: None,
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
            turn: None,
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

    // ── Turn markers (#2135) ─────────────────────────────────────────────────

    /// An `event_msg` line with the given payload type, as Codex writes them.
    fn marker(kind: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-09-25T00:00:01Z",
            "type": "event_msg",
            "payload": { "type": kind, "turn_id": "turn-1" },
        })
        .to_string()
    }

    fn started() -> String {
        marker("task_started")
    }

    fn complete() -> String {
        marker("task_complete")
    }

    fn aborted() -> String {
        marker("turn_aborted")
    }

    /// A conversation line of about `bytes` bytes that is not a turn marker.
    fn chatter(bytes: usize) -> String {
        serde_json::json!({
            "timestamp": "2026-09-25T00:00:02Z",
            "type": "response_item",
            "payload": { "type": "message", "role": "assistant", "content": "x".repeat(bytes) },
        })
        .to_string()
    }

    /// A conversation line whose text names every marker.
    fn quoting() -> String {
        serde_json::json!({
            "type": "response_item",
            "payload": { "type": "message", "content": "task_started task_complete turn_aborted" },
        })
        .to_string()
    }

    /// The turn `read_last_turn` finds in the rollout at `path`.
    fn turn_at(path: &Path) -> Option<RolloutTurn> {
        read_last_turn(path).unwrap().map(|marker| marker.turn)
    }

    /// The marker `read_last_turn` finds in a rollout holding a head and `lines`.
    fn turn_of(lines: &[String]) -> Option<RolloutTurn> {
        let tmp = tempfile::tempdir().unwrap();
        let mut all = vec![head(ID, serde_json::json!("cli"))];
        all.extend_from_slice(lines);
        turn_at(&write_rollout(tmp.path(), ID, &all))
    }

    fn turns(actions: &[Action]) -> Vec<Option<RolloutTurn>> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Seen { turn, .. } => Some(*turn),
                Action::End { .. } => None,
            })
            .collect()
    }

    fn track_of<'a>(state: &'a ScanState, path: &Path) -> &'a SessionTrack {
        match &state[path] {
            Tracked::Session(track) => track,
            other => panic!("not a session: {other:?}"),
        }
    }

    /// Applies `actions` to `registry` and returns the one session's state.
    fn state_after(registry: &SessionsRegistry, actions: Vec<Action>) -> SessionState {
        for action in actions {
            action.apply(registry);
        }
        registry.list()[0].state
    }

    #[test]
    fn only_an_event_msg_marker_counts_as_a_turn_marker() {
        assert_eq!(parse_turn_marker(&started()), Some(RolloutTurn::Open));
        assert_eq!(parse_turn_marker(&complete()), Some(RolloutTurn::Closed));
        assert_eq!(parse_turn_marker(&aborted()), Some(RolloutTurn::Closed));
        // A conversation line that quotes the names is not a marker, nor is an
        // `event_msg` of another kind that mentions one.
        assert_eq!(parse_turn_marker(&quoting()), None);
        let other = serde_json::json!({
            "type": "event_msg",
            "payload": { "type": "agent_message", "message": "task_started" },
        });
        assert_eq!(parse_turn_marker(&other.to_string()), None);
        // Nothing here is the schema the reader understands.
        for line in [
            "task_started",
            "",
            r#"{"type":"event_msg"}"#,
            r#"{"type":"event_msg","payload":"task_started"}"#,
            r#"{"type":"event_msg","payload":{"n":"task_started"}}"#,
            r#"{"type":"event_msg","payload":{"type":7,"n":"task_started"}}"#,
            r#"{"type":5,"payload":{"type":"task_started"}}"#,
        ] {
            assert_eq!(parse_turn_marker(line), None, "{line}");
        }
    }

    #[test]
    fn the_last_marker_wins_whatever_the_counts() {
        let cases: Vec<(Vec<String>, Option<RolloutTurn>)> = vec![
            (vec![started()], Some(RolloutTurn::Open)),
            (vec![started(), complete()], Some(RolloutTurn::Closed)),
            (vec![started(), aborted()], Some(RolloutTurn::Closed)),
            (
                vec![started(), complete(), started()],
                Some(RolloutTurn::Open),
            ),
            // Seven corpus files carry one completion too many.
            (
                vec![started(), complete(), complete()],
                Some(RolloutTurn::Closed),
            ),
            // Conversation after the marker, quoting markers, changes nothing.
            (
                vec![started(), chatter(10), quoting(), chatter(10)],
                Some(RolloutTurn::Open),
            ),
            (vec![chatter(10), quoting()], None),
            (vec![], None),
        ];
        for (lines, expected) in cases {
            assert_eq!(turn_of(&lines), expected, "{lines:?}");
        }
    }

    #[test]
    fn an_unreadable_rollout_is_an_error_and_a_markerless_one_is_not() {
        // A failed read is retried; a read that found no marker is final.
        assert!(read_last_turn(Path::new("/nonexistent/rollout.jsonl")).is_err());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(turn_at(&path), None);
        std::fs::write(&path, b"\xff\xfe\x00 not a rollout \n\xc3\x28\n").unwrap();
        assert_eq!(turn_at(&path), None);
    }

    #[test]
    fn a_final_line_still_being_written_is_passed_over() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        let head_line = head(ID, serde_json::json!("cli"));
        // A whole marker whose newline has not landed yet still counts.
        std::fs::write(&path, format!("{head_line}\n{}\n{}", started(), complete())).unwrap();
        assert_eq!(turn_at(&path), Some(RolloutTurn::Closed));
        // Half of one does not, so the marker before it stands.
        let done = complete();
        let half = &done[..done.len() / 2];
        std::fs::write(&path, format!("{head_line}\n{}\n{half}", started())).unwrap();
        assert_eq!(turn_at(&path), Some(RolloutTurn::Open));
    }

    #[test]
    fn a_window_starting_mid_line_drops_its_first_fragment() {
        let window = format!("{}\n{}\n", complete(), chatter(10));
        assert_eq!(
            last_turn_marker(window.as_bytes(), false),
            Some((RolloutTurn::Closed, 0))
        );
        // Past the start of the file, the first line is the tail of some other.
        assert_eq!(last_turn_marker(window.as_bytes(), true), None);
        assert_eq!(
            last_turn_marker(b"no newline at all task_started", true),
            None
        );
        // A line that is not UTF-8 is skipped, not fatal.
        let mut bytes = b"\xff\xfe\n".to_vec();
        bytes.extend_from_slice(started().as_bytes());
        assert_eq!(
            last_turn_marker(&bytes, false),
            Some((RolloutTurn::Open, 3))
        );
    }

    #[test]
    fn a_marker_is_located_by_the_offset_of_its_line_in_the_file() {
        // The offset is absolute even when the window starts past byte 0, because
        // it is how a marker already declined is told from a newer one.
        let tmp = tempfile::tempdir().unwrap();
        let lines = [
            head(ID, serde_json::json!("cli")),
            started(),
            chatter(300 * 1024),
            complete(),
        ];
        let path = write_rollout(tmp.path(), ID, &lines);
        let content = std::fs::read_to_string(&path).unwrap();
        let marker = read_last_turn(&path).unwrap().unwrap();
        assert_eq!(marker.turn, RolloutTurn::Closed);
        assert_eq!(marker.at, content.rfind(&complete()).unwrap() as u64);
        assert!(marker.at > TURN_TAIL_START, "found with a mid-file window");
        // A window starting at the file's start counts from zero.
        std::fs::write(&path, format!("{}\n", started())).unwrap();
        assert_eq!(read_last_turn(&path).unwrap().unwrap().at, 0);
    }

    #[test]
    fn the_tail_window_widens_to_find_a_distant_marker() {
        // 400 KiB of conversation after the marker: past the first 256 KiB
        // window, inside the second.
        let mut lines = vec![started()];
        lines.extend((0..4).map(|_| chatter(100 * 1024)));
        assert_eq!(turn_of(&lines), Some(RolloutTurn::Open));
        // One 3 MiB line of tool output after it: the first two windows lie
        // wholly inside that line and see no marker at all.
        assert_eq!(
            turn_of(&[complete(), chatter(3 * 1024 * 1024)]),
            Some(RolloutTurn::Closed)
        );
    }

    #[test]
    fn a_marker_beyond_the_widest_window_is_unknown() {
        let mut lines = vec![started()];
        lines.extend((0..50).map(|_| chatter(100 * 1024)));
        assert_eq!(turn_of(&lines), None);
    }

    #[test]
    fn a_hookless_session_reads_working_mid_turn_and_idle_after_it() {
        // The acceptance criterion of #2135, end to end through the registry.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started()],
        );
        let lock = RefCell::new(LockState::Held);
        let registry = SessionsRegistry::new();
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        let scan_at = |state: &mut ScanState, n: u32| {
            scan(
                tmp.path(),
                tmp.path(),
                state,
                t0 + WATCH_INTERVAL * n,
                &scripted(&lock),
            )
        };

        let first = scan_at(&mut state, 0);
        assert_eq!(turns(&first), vec![Some(RolloutTurn::Open)]);
        assert_eq!(state_after(&registry, first), SessionState::Working);

        // The turn goes on writing; nothing about it has changed.
        append(&path, &chatter(64));
        assert!(scan_at(&mut state, 1).is_empty());

        append(&path, &complete());
        let done = scan_at(&mut state, 2);
        assert_eq!(turns(&done), vec![Some(RolloutTurn::Closed)]);
        assert_eq!(state_after(&registry, done), SessionState::Idle);

        // The next turn, and one that is interrupted.
        append(&path, &started());
        let next = scan_at(&mut state, 3);
        assert_eq!(state_after(&registry, next), SessionState::Working);
        append(&path, &aborted());
        let interrupted = scan_at(&mut state, 4);
        assert_eq!(turns(&interrupted), vec![Some(RolloutTurn::Closed)]);
        assert_eq!(state_after(&registry, interrupted), SessionState::Idle);
    }

    #[test]
    fn a_closed_turn_on_first_sight_claims_nothing() {
        // The first read must not say `idle` over what a hook already reported.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started(), complete()],
        );
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        let first = scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        assert_eq!(turns(&first), vec![None]);
        assert_eq!(
            track_of(&state, &path).turn.map(|m| m.turn),
            Some(RolloutTurn::Closed)
        );

        let registry = SessionsRegistry::new();
        registry.observe(ObserveRequest {
            agent_id: None,
            pid: None,
            agent: Agent::Codex,
            session_id: ID.to_string(),
            cwd: None,
            transcript_path: None,
            event: SessionEvent::PreToolUse,
            repo: None,
            model: None,
        });
        assert_eq!(state_after(&registry, first), SessionState::Working);

        // The next turn is a change, and is reported.
        append(&path, &started());
        let next = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + WATCH_INTERVAL,
            &scripted(&lock),
        );
        assert_eq!(turns(&next), vec![Some(RolloutTurn::Open)]);
    }

    #[test]
    fn an_open_turn_in_a_stale_rollout_is_claimed_only_when_a_new_turn_starts() {
        // A crash leaves a turn open for good, and `codex resume` of it holds the
        // lock again: an old open marker is no evidence a turn is running, and
        // writing around it does not make it one. Only a newer marker does.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started()],
        );
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let far = SystemTime::now() + RECENT_ACTIVITY_WINDOW * 10;
        let first = scan(tmp.path(), tmp.path(), &mut state, far, &scripted(&lock));
        // Listed (its lock is held), but with no claim about its turn.
        assert_eq!(turns(&first), vec![None]);
        assert_eq!(track_of(&state, &path).turn, None);
        // Written to, but the marker is the same one.
        append(&path, &chatter(64));
        let grown = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            SystemTime::now(),
            &scripted(&lock),
        );
        assert!(turns(&grown).is_empty());
        // A new turn starts: that is evidence.
        append(&path, &complete());
        append(&path, &started());
        let started_again = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            SystemTime::now(),
            &scripted(&lock),
        );
        assert_eq!(turns(&started_again), vec![Some(RolloutTurn::Open)]);
    }

    #[test]
    fn no_turn_is_read_unless_the_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started()],
        );
        // A Codex without locks makes no claim about the thread.
        let lock = RefCell::new(LockState::Unknown);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        let first = scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        assert_eq!(turns(&first), vec![None]);
        assert_eq!(track_of(&state, &path).turn_read_at, None);
        // Once the lock is held it is read at once, with no growth needed.
        *lock.borrow_mut() = LockState::Held;
        let held = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + WATCH_INTERVAL,
            &scripted(&lock),
        );
        assert_eq!(turns(&held), vec![Some(RolloutTurn::Open)]);
        // A thread whose lock is already free or gone is not listed at all, so
        // its marker is not read.
        for gone in [LockState::Free, LockState::Absent] {
            let tmp = tempfile::tempdir().unwrap();
            let path = write_rollout(
                tmp.path(),
                ID,
                &[head(ID, serde_json::json!("cli")), started()],
            );
            let lock = RefCell::new(gone);
            let mut state = ScanState::new();
            assert!(scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock)).is_empty());
            assert_eq!(track_of(&state, &path).turn_read_at, None, "{gone:?}");
        }
    }

    #[test]
    fn an_unchanged_rollout_is_not_read_again() {
        assert_eq!(
            started().len(),
            aborted().len(),
            "the rewrite below keeps the size"
        );
        let tmp = tempfile::tempdir().unwrap();
        let head_line = head(ID, serde_json::json!("cli"));
        let path = write_rollout(tmp.path(), ID, &[head_line.clone(), started()]);
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        let first = scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        assert_eq!(turns(&first), vec![Some(RolloutTurn::Open)]);
        // Rewrite the marker in place at the same size: a re-read would see it
        // closed, so silence shows the tail was not read.
        std::fs::write(&path, format!("{head_line}\n{}\n", aborted())).unwrap();
        let again = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + WATCH_INTERVAL,
            &scripted(&lock),
        );
        assert!(again.is_empty());
    }

    #[test]
    fn a_turn_change_is_reported_at_once_and_counts_as_the_heartbeat() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started()],
        );
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        append(&path, &complete());
        let t1 = t0 + WATCH_INTERVAL;
        assert_eq!(
            turns(&scan(
                tmp.path(),
                tmp.path(),
                &mut state,
                t1,
                &scripted(&lock)
            )),
            vec![Some(RolloutTurn::Closed)],
            "not held back for a heartbeat"
        );
        // A heartbeat from the first report would be due now; from this one it
        // is not, because the change was a report.
        assert!(scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + HEARTBEAT_INTERVAL,
            &scripted(&lock)
        )
        .is_empty());
        let later = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t1 + HEARTBEAT_INTERVAL,
            &scripted(&lock),
        );
        assert_eq!(turns(&later), vec![None]);
    }

    #[test]
    fn a_marker_out_of_reach_leaves_the_established_turn_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started()],
        );
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        // The turn runs long enough to push its marker beyond the widest window.
        for _ in 0..50 {
            append(&path, &chatter(100 * 1024));
        }
        let grown = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + WATCH_INTERVAL,
            &scripted(&lock),
        );
        assert!(turns(&grown).is_empty(), "unknown changes nothing");
        assert_eq!(
            track_of(&state, &path).turn.map(|m| m.turn),
            Some(RolloutTurn::Open)
        );
    }

    #[test]
    fn a_resumed_session_does_not_claim_the_open_turn_it_died_in() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started()],
        );
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        let live = scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        assert_eq!(turns(&live), vec![Some(RolloutTurn::Open)]);
        // The process dies mid-turn: the kernel drops its lock.
        *lock.borrow_mut() = LockState::Absent;
        let ended = scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        assert_eq!(ids(&ended), vec![format!("end:{ID}")]);
        // `codex resume`: the rollout still ends in that open marker, and writing
        // around it is not a turn.
        *lock.borrow_mut() = LockState::Held;
        append(&path, &chatter(64));
        let resumed = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + WATCH_INTERVAL,
            &scripted(&lock),
        );
        assert_eq!(turns(&resumed), vec![None]);
        // Its first real turn is.
        append(&path, &started());
        let next = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + WATCH_INTERVAL * 2,
            &scripted(&lock),
        );
        assert_eq!(turns(&next), vec![Some(RolloutTurn::Open)]);
    }

    #[test]
    fn a_session_resumed_after_a_finished_turn_starts_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            tmp.path(),
            ID,
            &[head(ID, serde_json::json!("cli")), started(), complete()],
        );
        let lock = RefCell::new(LockState::Held);
        let mut state = ScanState::new();
        let t0 = SystemTime::now();
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        *lock.borrow_mut() = LockState::Absent;
        scan(tmp.path(), tmp.path(), &mut state, t0, &scripted(&lock));
        *lock.borrow_mut() = LockState::Held;
        append(&path, &started());
        let resumed = scan(
            tmp.path(),
            tmp.path(),
            &mut state,
            t0 + WATCH_INTERVAL,
            &scripted(&lock),
        );
        assert_eq!(turns(&resumed), vec![Some(RolloutTurn::Open)]);
    }

    #[test]
    fn a_failed_read_is_retried_not_remembered_as_done() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        let now = SystemTime::now();
        let mut track = SessionTrack {
            id: ID.to_string(),
            cwd: None,
            size: 10,
            lock_seen: true,
            ended: false,
            last_report: SystemTime::UNIX_EPOCH,
            turn: None,
            turn_read_at: None,
            stale_open: None,
        };
        let rollout = Rollout {
            path: &path,
            size: 10,
            modified: now,
        };
        // The file is not there to read: nothing is claimed, and nothing is
        // remembered, so the same size is read again.
        assert_eq!(track.refresh_turn(rollout, LockState::Held, now), None);
        assert_eq!(track.turn_read_at, None);
        std::fs::write(&path, format!("{}\n", started())).unwrap();
        assert_eq!(
            track.refresh_turn(rollout, LockState::Held, now),
            Some(RolloutTurn::Open)
        );
        assert_eq!(track.turn_read_at, Some(10));
    }

    #[test]
    fn a_turn_action_applies_as_passive_evidence() {
        let registry = SessionsRegistry::new();
        let seen = |turn| Action::Seen {
            id: ID.to_string(),
            cwd: Some(PathBuf::from("/work/repo")),
            transcript_path: PathBuf::from("/r.jsonl"),
            turn,
        };
        seen(Some(RolloutTurn::Open)).apply(&registry);
        let listed = registry.list();
        assert_eq!(listed[0].agent, Agent::Codex);
        assert_eq!(listed[0].state, SessionState::Working);
        assert_eq!(
            listed[0].last_event,
            SessionEvent::RolloutTurn(RolloutTurn::Open)
        );
        // A plain sighting keeps the state; a closed turn ends it.
        seen(None).apply(&registry);
        assert_eq!(registry.list()[0].state, SessionState::Working);
        seen(Some(RolloutTurn::Closed)).apply(&registry);
        assert_eq!(registry.list()[0].state, SessionState::Idle);
    }
}
