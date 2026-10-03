//! The daemon side of the hook journals (#2108): replay them at startup, tail
//! them while running, and clean them up.
//!
//! The hook sink appends every event to its session's journal before it posts it
//! ([`journal`]). The socket stays the fast path; this watcher is the durable
//! one. It takes what the socket missed — a session that started while the daemon
//! was restarting, an event whose POST timed out — and, after a restart, brings
//! back the sessions that are still alive in the state they were left in.
//!
//! **Hooks own the events; the daemon owns the state.** Nothing here computes a
//! state. Records are fed through the registry's ordinary
//! [`observe_stamped`](SessionsRegistry::observe_stamped) /
//! [`end_stamped`](SessionsRegistry::end_stamped), so the same state machine, the
//! same replaced-pid rule (#1948) and the same subagent roll-up decide the result
//! whichever route an event arrived by. A stamp (`ts` + `seq`) lets the registry
//! drop the copy of an event it already has, and a late-read event that is older
//! than one already applied.
//!
//! **Poll, not watch.** One pass every [`WATCH_INTERVAL`] — the cadence of the
//! transcript and Codex rollout watchers — in its own task. There is no
//! file-notification dependency, a poll is trivial to test and behaves the same on
//! macOS and Linux, and the POST remains the fast path, so the interval only
//! bounds how late a *dropped* event is caught. A pass lists the two agent
//! directories, `stat`s each file, and reads only the bytes that were appended.
//!
//! **A file is judged the first time it is seen** — every file at startup, and
//! one that appears later. Its records are replayed into a scratch registry to see
//! what the real state machine makes of them, then:
//!
//! - a session that has *ended* is dropped (its `SessionEnd` was journaled while
//!   the daemon was down), and so is one whose last event is older than
//!   [`MAX_REPLAY_AGE`];
//! - otherwise it needs **proof of life**: a Codex writer lock that is held, or
//!   the owning pid still running *and* having started no later than the first
//!   event journaled from it (the daemon never saw a token for it, so this is the
//!   check that stops a recycled pid vouching for a dead session). A held Codex
//!   lock that is free or gone is proof of death. A dead or unreadable *pid* is
//!   not: it might be a per-hook shell, so it only withholds the proof, exactly as
//!   the [pid watcher](super::pid_watcher) never trusts a pid it has not seen
//!   alive;
//! - without proof a session is kept only if its last event is within the session
//!   TTL, which is what a hook-fed session with no pid always had.
//!
//! An accepted session is replayed into the real registry with each event's own
//! timestamp as its wall-clock `last_seen`, so replayed history cannot look fresh.
//! Its awake-time TTL stamp is *now*, because the proof was just established; that
//! is what lets it survive until the pid watcher's first confirmation, and why
//! this depends on the TTL being measured in awake time.
//!
//! **Cleanup is the daemon's.** A rejected journal is deleted at once. An accepted
//! one is deleted when the registry no longer holds its session live and the file
//! has been quiet for [`ORPHAN_GRACE`], or when it is older than
//! [`MAX_REPLAY_AGE`] whatever the registry says. A journal over
//! [`MAX_JOURNAL_BYTES`] is compacted to its tail by an atomic rename; that is
//! abandoned if the file grew since it was read, so the only events it can lose
//! are ones that land in the instant between the check and the rename, which the
//! POST has already delivered in all but the daemon-down case.
//!
//! Only regular files named `<lowercase uuid>.jsonl` are ever read or removed, a
//! symlink is never followed, and nothing read is logged beyond a session id.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::codex_watcher::{self, LockState};
use super::journal::{self, JournalBody, JournalRecord};
use super::{
    pid_liveness, Agent, EventStamp, ObserveRequest, Origin, SessionEntry, SessionState,
    SessionsRegistry, DEFAULT_SESSION_TTL,
};

/// How often the journals are scanned. Matches the other session watchers.
const WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// A journal whose last event is older than this is not replayed, and one whose
/// file is older is deleted whatever the registry holds. Generous: a session left
/// open and idle for days is real, and a live pid is cheap proof.
const MAX_REPLAY_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long a journal of a session the registry does not hold live must have been
/// quiet before it is deleted: past the ended-linger window, and long enough that
/// a file imported a moment ago is never judged against a registry snapshot that
/// predates it.
const ORPHAN_GRACE: Duration = Duration::from_secs(120);

/// A journal above this size is compacted.
const MAX_JOURNAL_BYTES: u64 = 128 * 1024;

/// What a compaction keeps: the tail, which is all a replay needs.
const COMPACT_TO_BYTES: u64 = 64 * 1024;

/// The most one read takes from a file. A bigger append backlog is read from its
/// tail, because replaying the end is what matters.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// How much later than a pid's first journaled event its process may have
/// started and still count as the same process: `ps` reports whole seconds.
const PID_START_SLACK: chrono::Duration = chrono::Duration::seconds(2);

/// How long a stray compaction temp file survives before it is swept.
const STALE_TMP_AGE: Duration = Duration::from_secs(60 * 60);

/// The agents whose hooks write journals.
const AGENTS: [Agent; 2] = [Agent::Claude, Agent::Codex];

/// The process and lock checks the replay's proof of life uses, so tests can
/// script them without real processes.
pub(crate) trait Probes {
    /// Whether a process with this pid exists.
    fn pid_exists(&self, pid: u32) -> bool;
    /// When the process started, if it can be read.
    fn pid_start_time(&self, pid: u32) -> Option<DateTime<Utc>>;
    /// The state of a Codex thread's writer lock.
    fn codex_lock(&self, session_id: &str) -> LockState;
}

/// The real checks.
struct RealProbes;

impl Probes for RealProbes {
    fn pid_exists(&self, pid: u32) -> bool {
        pid_liveness::process_exists(pid)
    }

    fn pid_start_time(&self, pid: u32) -> Option<DateTime<Utc>> {
        pid_liveness::process_start_time(pid)
    }

    fn codex_lock(&self, session_id: &str) -> LockState {
        codex_watcher::probe_thread_lock(session_id)
    }
}

/// Resolves a `cwd` to its repository name; the daemon passes its git
/// enrichment, so the engine itself stays free of git.
pub(crate) type Enrich = Arc<dyn Fn(&Path) -> Option<String> + Send + Sync>;

/// What the proof of life found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    /// The session's process is running.
    Alive,
    /// The session's process is gone (a Codex lock that is free or absent).
    Dead,
    /// Nothing either way.
    Unknown,
}

/// Why a journal was not replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reject {
    /// The session ended while the daemon was away.
    Ended,
    /// Its last event is older than [`MAX_REPLAY_AGE`].
    TooOld,
    /// Its process is gone.
    Dead,
    /// No proof of life and its last event is older than the session TTL.
    Stale,
}

/// The verdict on a journal seen for the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Replay it. `proven` is whether the session's process vouched for it.
    Accept { proven: bool },
    /// Delete it.
    Reject(Reject),
}

/// One thing a scan asks the registry to do.
#[derive(Debug, Clone)]
pub(crate) enum Action {
    /// Apply an `observe`.
    Observe {
        req: ObserveRequest,
        stamp: EventStamp,
    },
    /// Apply a `SessionEnd`.
    End {
        session_id: String,
        reason: Option<String>,
        pid: Option<u32>,
        stamp: EventStamp,
    },
}

impl Action {
    fn apply(self, registry: &SessionsRegistry) {
        match self {
            Self::Observe { req, stamp } => {
                registry.observe_stamped(req, Some(stamp), Origin::Journal);
            }
            Self::End {
                session_id,
                reason,
                pid,
                stamp,
            } => {
                registry.end_stamped(
                    &session_id,
                    reason.as_deref(),
                    pid,
                    Some(stamp),
                    Origin::Journal,
                );
            }
        }
    }
}

/// What the watcher remembers about a journal it accepted.
#[derive(Debug, Clone)]
struct FileTrack {
    /// The session, the lower-case id the file is named for.
    session_id: String,
    /// The inode, so a compacted (renamed-over) file is noticed.
    ino: u64,
    /// How many bytes, through the last complete line, have been handed over.
    offset: u64,
    /// The scan that accepted it; a file is never swept the pass it is imported.
    imported_in: u64,
}

/// The watcher's state across scans.
#[derive(Debug, Default)]
pub(crate) struct JournalState {
    /// Accepted journals by path.
    files: HashMap<PathBuf, FileTrack>,
    /// A bounded cache of `cwd` → repo, so a tailed tool-call event does not run
    /// git discovery each time.
    repos: HashMap<PathBuf, Option<String>>,
    /// The number of the current scan.
    scan: u64,
}

/// One journal file on disk.
#[derive(Debug, Clone)]
struct JournalFile {
    path: PathBuf,
    /// The lower-case session id the file is named for.
    id: String,
    len: u64,
    ino: u64,
    mtime: SystemTime,
}

/// A bounded `cwd` cache size.
const MAX_REPO_CACHE: usize = 256;

/// One scan's working set.
struct Scan<'a> {
    state: &'a mut JournalState,
    now: SystemTime,
    live: &'a HashSet<String>,
    probes: &'a dyn Probes,
    enrich: &'a dyn Fn(&Path) -> Option<String>,
    actions: Vec<Action>,
}

impl Scan<'_> {
    /// How long ago `file` was last written.
    fn quiet_for(&self, file: &JournalFile) -> Duration {
        self.now.duration_since(file.mtime).unwrap_or_default()
    }

    /// The repo for `cwd`, from the cache or the enricher.
    fn repo_for(&mut self, cwd: &Path) -> Option<String> {
        if let Some(hit) = self.state.repos.get(cwd) {
            return hit.clone();
        }
        let repo = (self.enrich)(cwd);
        if self.state.repos.len() >= MAX_REPO_CACHE {
            self.state.repos.clear();
        }
        self.state.repos.insert(cwd.to_path_buf(), repo.clone());
        repo
    }

    /// Turns `records` into actions, oldest first.
    fn emit(&mut self, mut records: Vec<JournalRecord>) {
        records.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.seq.cmp(&b.seq)));
        for record in records {
            let stamp = record.stamp();
            match &record.body {
                JournalBody::End { reason, pid } => self.actions.push(Action::End {
                    session_id: record.session_id.clone(),
                    reason: reason.clone(),
                    pid: *pid,
                    stamp,
                }),
                JournalBody::Observe { cwd, .. } => {
                    let repo = cwd.as_deref().and_then(|cwd| self.repo_for(cwd));
                    if let Some(mut req) = record.to_observe_request() {
                        req.repo = repo;
                        self.actions.push(Action::Observe { req, stamp });
                    }
                }
            }
        }
    }
}

/// Replays `records` into a scratch registry and judges the session. Pure apart
/// from the probes.
fn assess(records: &[JournalRecord], now: DateTime<Utc>, probes: &dyn Probes) -> Verdict {
    let Some(last) = records.iter().map(|r| r.ts).max() else {
        return Verdict::Reject(Reject::Stale);
    };
    let age = (now - last).to_std().unwrap_or_default();
    if age > MAX_REPLAY_AGE {
        return Verdict::Reject(Reject::TooOld);
    }
    let Some(entry) = replay_scratch(records) else {
        return Verdict::Reject(Reject::Ended);
    };
    if entry.state == SessionState::Ended {
        return Verdict::Reject(Reject::Ended);
    }
    match prove(&entry, records, probes) {
        Proof::Alive => Verdict::Accept { proven: true },
        Proof::Dead => Verdict::Reject(Reject::Dead),
        Proof::Unknown if age <= DEFAULT_SESSION_TTL => Verdict::Accept { proven: false },
        Proof::Unknown => Verdict::Reject(Reject::Stale),
    }
}

/// The entry the registry's state machine leaves a session in after `records`,
/// or `None` when they leave no live entry (an `end` for a session never seen).
fn replay_scratch(records: &[JournalRecord]) -> Option<SessionEntry> {
    let scratch = SessionsRegistry::scratch();
    let mut sorted: Vec<&JournalRecord> = records.iter().collect();
    sorted.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.seq.cmp(&b.seq)));
    let id = sorted.first()?.session_id.clone();
    for record in sorted {
        match &record.body {
            JournalBody::End { reason, pid } => {
                scratch.end_stamped(
                    &record.session_id,
                    reason.as_deref(),
                    *pid,
                    Some(record.stamp()),
                    Origin::Journal,
                );
            }
            JournalBody::Observe { .. } => {
                if let Some(req) = record.to_observe_request() {
                    scratch.observe_stamped(req, Some(record.stamp()), Origin::Journal);
                }
            }
        }
    }
    let entry = scratch.lock_sessions().get(&id).cloned();
    entry
}

/// What the world says about a replayed session's process.
fn prove(entry: &SessionEntry, records: &[JournalRecord], probes: &dyn Probes) -> Proof {
    if entry.agent == Agent::Codex {
        match probes.codex_lock(&entry.session_id.to_ascii_lowercase()) {
            LockState::Held => return Proof::Alive,
            LockState::Free | LockState::Absent => return Proof::Dead,
            LockState::Unknown => {}
        }
    }
    let Some(pid) = entry.pid else {
        return Proof::Unknown;
    };
    let Some(first) = records
        .iter()
        .filter(|r| r.pid() == Some(pid))
        .map(|r| r.ts)
        .min()
    else {
        return Proof::Unknown;
    };
    if !probes.pid_exists(pid) {
        return Proof::Unknown;
    }
    match probes.pid_start_time(pid) {
        // Already running when it wrote its first event: the same process. One
        // that started later is a recycled pid, which vouches for nothing.
        Some(started) if started <= first + PID_START_SLACK => Proof::Alive,
        _ => Proof::Unknown,
    }
}

/// The records a read produced and how far it got.
struct Chunk {
    records: Vec<JournalRecord>,
    /// The offset just past the last complete line read.
    new_offset: u64,
}

/// Reads and parses the complete lines of `path` from `from` to `len`. A backlog
/// over [`MAX_READ_BYTES`] is read from its tail, skipping the line it lands in.
fn read_chunk(path: &Path, from: u64, len: u64) -> std::io::Result<Chunk> {
    let start = if len.saturating_sub(from) > MAX_READ_BYTES {
        len - MAX_READ_BYTES
    } else {
        from
    };
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(len.saturating_sub(start)).read_to_end(&mut buf)?;
    // Landed mid-line: drop the fragment up to the first newline.
    let skipped = if start > from {
        buf.iter()
            .position(|b| *b == b'\n')
            .map_or(buf.len(), |i| i + 1)
    } else {
        0
    };
    let body = &buf[skipped..];
    let complete = body.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let records = String::from_utf8_lossy(&body[..complete])
        .lines()
        .filter_map(JournalRecord::parse)
        .collect();
    let new_offset = start + (skipped + complete) as u64;
    Ok(Chunk {
        records,
        new_offset,
    })
}

/// The files in `dir`: regular `<lowercase uuid>.jsonl` files only, with stray
/// compaction temp files swept along the way.
fn list_journals(dir: &Path, now: SystemTime) -> Vec<JournalFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // `symlink_metadata`: a link is never followed, so a journal directory
        // cannot be used to make the daemon read or delete something else.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if name.starts_with('.') && name.ends_with(".tmp") {
            if now.duration_since(mtime).unwrap_or_default() > STALE_TMP_AGE {
                remove(&path, "stale_tmp");
            }
            continue;
        }
        let Some(id) = journal_id(name) else {
            continue;
        };
        files.push(JournalFile {
            path,
            id,
            len: meta.len(),
            ino: meta.ino(),
            mtime,
        });
    }
    files
}

/// The session id a journal file name carries: `<lowercase uuid>.jsonl`.
fn journal_id(name: &str) -> Option<String> {
    let id = name.strip_suffix(".jsonl")?;
    (journal::is_session_uuid(id) && id == id.to_ascii_lowercase()).then(|| id.to_string())
}

/// Deletes a journal file, logging a failure at debug.
fn remove(path: &Path, reason: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(reason, "session_journal_removed"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::debug!(reason, %error, "session_journal_remove_failed"),
    }
}

/// Keeps only the records that belong to `agent` and `id`: a file holds one
/// session, so anything else in it is not trusted.
fn belonging(records: Vec<JournalRecord>, agent: Agent, id: &str) -> Vec<JournalRecord> {
    records
        .into_iter()
        .filter(|r| r.agent == agent && r.session_id.eq_ignore_ascii_case(id))
        .collect()
}

/// A scan over the journal tree under `dir`, returning what the registry should
/// hear. `live` is the registry's live session ids as of just before the scan.
/// Pure apart from the file I/O and the injected probes.
pub(crate) fn scan(
    dir: &Path,
    state: &mut JournalState,
    now: SystemTime,
    live: &HashSet<String>,
    probes: &dyn Probes,
    enrich: &dyn Fn(&Path) -> Option<String>,
) -> Vec<Action> {
    state.scan += 1;
    let mut scan = Scan {
        state,
        now,
        live,
        probes,
        enrich,
        actions: Vec::new(),
    };
    let mut present = HashSet::new();
    for agent in AGENTS {
        let Some(name) = journal::agent_dir_name(agent) else {
            continue;
        };
        for file in list_journals(&dir.join(name), now) {
            present.insert(file.path.clone());
            visit(&mut scan, agent, &file);
        }
    }
    scan.state.files.retain(|path, _| present.contains(path));
    scan.actions
}

/// Handles one journal file: judge it if new, otherwise tail it; then sweep and
/// compact it.
fn visit(scan: &mut Scan<'_>, agent: Agent, file: &JournalFile) {
    let track = match scan.state.files.remove(&file.path) {
        None => import(scan, agent, file),
        Some(track) => Some(tail(scan, agent, file, track)),
    };
    let Some(mut track) = track else {
        return;
    };
    if swept(scan, file, &track) {
        return;
    }
    compact_if_needed(file, &mut track);
    scan.state.files.insert(file.path.clone(), track);
}

/// Judges a journal seen for the first time, and replays it if it passes.
fn import(scan: &mut Scan<'_>, agent: Agent, file: &JournalFile) -> Option<FileTrack> {
    let chunk = match read_chunk(&file.path, 0, file.len) {
        Ok(chunk) => chunk,
        Err(error) => {
            tracing::debug!(session_id = %file.id, %error, "session_journal_read_failed");
            return None;
        }
    };
    let records = belonging(chunk.records, agent, &file.id);
    if records.is_empty() {
        // Empty (a hook is between creating it and writing) or junk: give a fresh
        // one another scan, delete a stale one.
        if scan.quiet_for(file) > ORPHAN_GRACE {
            remove(&file.path, "unreadable");
        }
        return None;
    }
    match assess(&records, DateTime::from(scan.now), scan.probes) {
        Verdict::Reject(reason) => {
            tracing::debug!(session_id = %file.id, ?reason, "session_journal_rejected");
            remove(&file.path, "rejected");
            None
        }
        Verdict::Accept { proven } => {
            tracing::debug!(session_id = %file.id, proven, records = records.len(), "session_journal_replayed");
            scan.emit(records);
            Some(FileTrack {
                session_id: file.id.clone(),
                ino: file.ino,
                offset: chunk.new_offset,
                imported_in: scan.state.scan,
            })
        }
    }
}

/// Hands over what was appended to an accepted journal since the last scan, or
/// the whole file again when it was rewritten (compacted); the registry's stamps
/// drop what it already has.
fn tail(scan: &mut Scan<'_>, agent: Agent, file: &JournalFile, mut track: FileTrack) -> FileTrack {
    let replaced = file.ino != track.ino;
    let shrank = track.offset > file.len;
    let rewritten = replaced || shrank;
    if !rewritten && file.len == track.offset {
        return track;
    }
    let from = if rewritten { 0 } else { track.offset };
    match read_chunk(&file.path, from, file.len) {
        Ok(chunk) => {
            scan.emit(belonging(chunk.records, agent, &file.id));
            track.offset = chunk.new_offset;
            track.ino = file.ino;
        }
        Err(error) => {
            tracing::debug!(session_id = %file.id, %error, "session_journal_read_failed");
        }
    }
    track
}

/// Deletes an accepted journal that is no longer wanted. Returns whether it did.
fn swept(scan: &Scan<'_>, file: &JournalFile, track: &FileTrack) -> bool {
    if track.imported_in == scan.state.scan {
        return false;
    }
    let quiet = scan.quiet_for(file);
    let reason = if quiet > MAX_REPLAY_AGE {
        "too_old"
    } else if quiet > ORPHAN_GRACE && !scan.live.contains(&track.session_id) {
        "orphan"
    } else {
        return false;
    };
    remove(&file.path, reason);
    true
}

/// Compacts a journal that has outgrown [`MAX_JOURNAL_BYTES`], when it is fully
/// consumed (so nothing unread is rewritten away).
fn compact_if_needed(file: &JournalFile, track: &mut FileTrack) {
    if file.len <= MAX_JOURNAL_BYTES || track.offset != file.len {
        return;
    }
    match compact(&file.path, file.len, file.ino) {
        Ok(Some((len, ino))) => {
            track.offset = len;
            track.ino = ino;
            tracing::debug!(session_id = %file.id, len, "session_journal_compacted");
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(session_id = %file.id, %error, "session_journal_compact_failed");
        }
    }
}

/// Rewrites the journal at `path` to its last [`COMPACT_TO_BYTES`], cut at a line
/// boundary, through a same-directory `0600` temp file and an atomic rename.
/// `len` and `ino` are what the caller saw; the rewrite is abandoned (`None`) if
/// the file has changed since, so an append is never silently dropped by a stale
/// read. Returns the new length and inode.
fn compact(path: &Path, len: u64, ino: u64) -> anyhow::Result<Option<(u64, u64)>> {
    use anyhow::Context;

    let start = len.saturating_sub(COMPACT_TO_BYTES);
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(len - start).read_to_end(&mut buf)?;
    let skip = if start > 0 {
        buf.iter()
            .position(|b| *b == b'\n')
            .map_or(buf.len(), |i| i + 1)
    } else {
        0
    };
    let tail = &buf[skip..];
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("a journal has a file name")?;
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    crate::daemon::paths::write_file_0600(&tmp, tail)?;
    let now = std::fs::metadata(path)?;
    if now.len() != len || now.ino() != ino {
        let _ = std::fs::remove_file(&tmp);
        return Ok(None);
    }
    std::fs::rename(&tmp, path)?;
    let after = std::fs::metadata(path)?;
    Ok(Some((after.len(), after.ino())))
}

/// Spawns the watcher loop, returning its [`JoinHandle`].
///
/// The first pass is the startup replay; every later one tails and cleans.
/// Directory walks, reads, process probes and git enrichment are blocking work,
/// so each pass runs on a blocking thread. Must be called from within a tokio
/// runtime.
pub(crate) fn spawn(
    registry: Arc<SessionsRegistry>,
    dir: PathBuf,
    enrich: Enrich,
    token: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::debug!("session journal watcher scanning {}", dir.display());
        let mut state = JournalState::default();
        loop {
            let live = registry.live_session_ids();
            let (scan_dir, scan_enrich) = (dir.clone(), enrich.clone());
            let mut owned = std::mem::take(&mut state);
            let (returned, actions) = tokio::task::spawn_blocking(move || {
                let actions = scan(
                    &scan_dir,
                    &mut owned,
                    SystemTime::now(),
                    &live,
                    &RealProbes,
                    &*scan_enrich,
                );
                (owned, actions)
            })
            .await
            .unwrap_or_else(|_| (JournalState::default(), Vec::new()));
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
    use crate::sessions::journal::JournalRecord;
    use crate::sessions::{NotificationKind, SessionEvent, Source};
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4c";
    const ID2: &str = "1c8f7d2b-3a5e-4b9f-8d4c-6e2f8a0b3c5d";

    /// Scripted probes: which pids run and since when, and what each Codex
    /// thread's lock says.
    #[derive(Default)]
    struct Fake {
        pids: RefCell<HashMap<u32, Option<DateTime<Utc>>>>,
        locks: RefCell<HashMap<String, LockState>>,
    }

    impl Fake {
        fn running(&self, pid: u32, since: DateTime<Utc>) {
            self.pids.borrow_mut().insert(pid, Some(since));
        }

        fn running_unreadable(&self, pid: u32) {
            self.pids.borrow_mut().insert(pid, None);
        }

        fn lock(&self, id: &str, state: LockState) {
            self.locks.borrow_mut().insert(id.to_string(), state);
        }
    }

    impl Probes for Fake {
        fn pid_exists(&self, pid: u32) -> bool {
            self.pids.borrow().contains_key(&pid)
        }

        fn pid_start_time(&self, pid: u32) -> Option<DateTime<Utc>> {
            self.pids.borrow().get(&pid).copied().flatten()
        }

        fn codex_lock(&self, id: &str) -> LockState {
            self.locks
                .borrow()
                .get(id)
                .copied()
                .unwrap_or(LockState::Unknown)
        }
    }

    fn ago(secs: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::seconds(secs)
    }

    fn rec(
        id: &str,
        agent: Agent,
        event: SessionEvent,
        ts: DateTime<Utc>,
        seq: &str,
        pid: Option<u32>,
    ) -> JournalRecord {
        JournalRecord {
            v: 1,
            ts,
            seq: seq.to_string(),
            agent,
            session_id: id.to_string(),
            body: JournalBody::Observe {
                event,
                agent_id: None,
                cwd: Some(PathBuf::from("/work/repo")),
                transcript_path: None,
                model: None,
                pid,
            },
        }
    }

    fn end_rec(id: &str, ts: DateTime<Utc>, seq: &str, pid: Option<u32>) -> JournalRecord {
        JournalRecord {
            v: 1,
            ts,
            seq: seq.to_string(),
            agent: Agent::Claude,
            session_id: id.to_string(),
            body: JournalBody::End { reason: None, pid },
        }
    }

    fn write(dir: &Path, records: &[JournalRecord]) -> PathBuf {
        for record in records {
            journal::append(dir, record).unwrap();
        }
        journal::journal_path(dir, records[0].agent, &records[0].session_id).unwrap()
    }

    fn enrich(cwd: &Path) -> Option<String> {
        (cwd == Path::new("/work/repo")).then(|| "repo".to_string())
    }

    fn run(
        dir: &Path,
        state: &mut JournalState,
        registry: &SessionsRegistry,
        probes: &Fake,
    ) -> Vec<Action> {
        let actions = scan(
            dir,
            state,
            SystemTime::now(),
            &registry.live_session_ids(),
            probes,
            &enrich,
        );
        for action in actions.clone() {
            action.apply(registry);
        }
        actions
    }

    fn entry(registry: &SessionsRegistry, id: &str) -> Option<SessionEntry> {
        registry.lock_sessions().get(id).cloned()
    }

    #[test]
    fn startup_replay_restores_state_cwd_repo_and_the_events_own_time() {
        let tmp = tempfile::tempdir().unwrap();
        let started = ago(3600);
        let path = write(
            tmp.path(),
            &[
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::SessionStart,
                    ago(1000),
                    "1-1",
                    Some(500),
                ),
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::UserPromptSubmit,
                    ago(900),
                    "1-2",
                    Some(500),
                ),
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::PreToolUse,
                    ago(800),
                    "1-3",
                    Some(500),
                ),
            ],
        );
        let probes = Fake::default();
        probes.running(500, started);
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);

        let restored = registry.list();
        assert_eq!(restored.len(), 1, "a live session comes back");
        let e = &restored[0];
        assert_eq!(e.state, SessionState::Working);
        assert_eq!(e.cwd.as_deref(), Some(Path::new("/work/repo")));
        assert_eq!(e.repo.as_deref(), Some("repo"));
        assert_eq!(e.agent, Agent::Claude);
        assert!(e.last_seen < ago(700), "history keeps its own timestamp");
        assert!(e.started_at < ago(900));
        assert!(entry(&registry, ID).unwrap().prompted);
        assert_eq!(entry(&registry, ID).unwrap().pid, Some(500));
        assert!(path.exists(), "an accepted journal is kept");
    }

    #[test]
    fn a_session_started_while_the_daemon_was_down_is_listed_with_its_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::SessionStart,
                ago(20),
                "1-1",
                None,
            )],
        );
        let registry = SessionsRegistry::new();
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, SessionState::Starting);
        assert_eq!(listed[0].cwd.as_deref(), Some(Path::new("/work/repo")));
        assert_eq!(listed[0].repo.as_deref(), Some("repo"));
    }

    #[test]
    fn an_ended_session_is_dropped_and_its_journal_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::PreToolUse,
                    ago(30),
                    "1-1",
                    Some(500),
                ),
                end_rec(ID, ago(10), "1-2", Some(500)),
            ],
        );
        let probes = Fake::default();
        probes.running(500, ago(3600));
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);
        assert!(
            registry.list().is_empty(),
            "a SessionEnd kept while the daemon was down"
        );
        assert!(!path.exists());
    }

    #[test]
    fn an_end_from_a_replaced_process_does_not_end_the_resumed_session() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            &[
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::SessionStart,
                    ago(100),
                    "1-1",
                    Some(500),
                ),
                // `--resume` in place: a new process takes the session over...
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::SessionStart,
                    ago(60),
                    "1-2",
                    Some(600),
                ),
                // ...and the old one's SessionEnd lands last (#1948).
                end_rec(ID, ago(50), "1-3", Some(500)),
            ],
        );
        let probes = Fake::default();
        probes.running(600, ago(3600));
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);
        let e = entry(&registry, ID).expect("the resumed session lives");
        assert_eq!(e.state, SessionState::Starting);
        assert_eq!(e.pid, Some(600));
    }

    #[test]
    fn a_dead_session_does_not_come_back() {
        let tmp = tempfile::tempdir().unwrap();
        // Last event a day ago, its pid long gone: no proof and too old for the TTL.
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(86_400),
                "1-1",
                Some(500),
            )],
        );
        let registry = SessionsRegistry::new();
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        assert!(registry.list().is_empty());
        assert!(!path.exists(), "a rejected journal is deleted at once");
    }

    #[test]
    fn a_live_idle_session_survives_however_long_it_has_been_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            &[
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::UserPromptSubmit,
                    ago(7300),
                    "1-1",
                    Some(500),
                ),
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::Stop,
                    ago(7200),
                    "1-2",
                    Some(500),
                ),
            ],
        );
        let probes = Fake::default();
        probes.running(500, ago(86_400));
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);
        let listed = registry.list();
        assert_eq!(listed.len(), 1, "proof of life beats the TTL");
        assert_eq!(listed[0].state, SessionState::Idle);
        assert!(listed[0].last_seen < ago(7000));
    }

    #[test]
    fn a_recycled_pid_vouches_for_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(7200),
                "1-1",
                Some(500),
            )],
        );
        let probes = Fake::default();
        // The pid runs, but it started an hour *after* the journal's first event.
        probes.running(500, ago(3600));
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);
        assert!(registry.list().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn an_unreadable_start_time_is_no_proof_but_not_a_death_either() {
        let tmp = tempfile::tempdir().unwrap();
        let probes = Fake::default();
        probes.running_unreadable(500);
        // Recent: kept on the TTL's say-so, as a pid-less hook session always was.
        write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(60),
                "1-1",
                Some(500),
            )],
        );
        // Stale: no proof and past the TTL.
        write(
            tmp.path(),
            &[rec(
                ID2,
                Agent::Claude,
                SessionEvent::Stop,
                ago(900),
                "2-1",
                Some(500),
            )],
        );
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);
        let ids: Vec<String> = registry.list().into_iter().map(|e| e.session_id).collect();
        assert_eq!(ids, vec![ID.to_string()]);
    }

    #[test]
    fn a_hook_pid_that_was_a_short_lived_shell_falls_back_to_the_ttl() {
        // The pid is gone — which a per-hook shell's would be — so the session is
        // judged by its age alone and is not declared dead.
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::PreToolUse,
                ago(30),
                "1-1",
                Some(500),
            )],
        );
        let registry = SessionsRegistry::new();
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn a_codex_session_is_judged_by_its_writer_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let held = "019a0000-0000-7000-8000-000000000001";
        let free = "019a0000-0000-7000-8000-000000000002";
        let unknown = "019a0000-0000-7000-8000-000000000003";
        let probes = Fake::default();
        probes.lock(held, LockState::Held);
        probes.lock(free, LockState::Free);
        // Held and a day old: alive. Free and a minute old: gone. No lock
        // information and a minute old: the TTL keeps it.
        for (id, age) in [(held, 86_400), (free, 60), (unknown, 60)] {
            write(
                tmp.path(),
                &[rec(
                    id,
                    Agent::Codex,
                    SessionEvent::PostToolUse,
                    ago(age),
                    "c-1",
                    None,
                )],
            );
        }
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);
        let mut ids: Vec<String> = registry.list().into_iter().map(|e| e.session_id).collect();
        ids.sort();
        assert_eq!(ids, vec![held.to_string(), unknown.to_string()]);
        assert!(registry.list().iter().all(|e| e.agent == Agent::Codex));
    }

    #[test]
    fn a_journal_older_than_the_replay_cap_is_rejected_even_with_proof() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(8 * 24 * 3600),
                "1-1",
                Some(500),
            )],
        );
        let probes = Fake::default();
        probes.running(500, ago(30 * 24 * 3600));
        let registry = SessionsRegistry::new();
        run(tmp.path(), &mut JournalState::default(), &registry, &probes);
        assert!(registry.list().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn replay_waits_out_a_system_sleep_like_any_other_session() {
        // The replayed session's TTL runs from now, in awake time.
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(60),
                "1-1",
                None,
            )],
        );
        let registry = SessionsRegistry::new();
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        registry.clock.advance(Duration::from_secs(299));
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn a_hook_appending_later_is_tailed_and_a_partial_line_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(30),
                "1-1",
                None,
            )],
        );
        let registry = SessionsRegistry::new();
        let mut state = JournalState::default();
        let probes = Fake::default();
        assert_eq!(run(tmp.path(), &mut state, &registry, &probes).len(), 1);
        assert!(
            run(tmp.path(), &mut state, &registry, &probes).is_empty(),
            "nothing new"
        );

        // A hook whose POST was dropped appends a working event.
        let next = rec(
            ID,
            Agent::Claude,
            SessionEvent::PreToolUse,
            ago(1),
            "1-2",
            None,
        );
        let line = format!("{}\n", serde_json::to_string(&next).unwrap());
        let (head, rest) = line.split_at(line.len() / 2);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(head.as_bytes()).unwrap();
        assert!(
            run(tmp.path(), &mut state, &registry, &probes).is_empty(),
            "half a line is not a record yet"
        );
        f.write_all(rest.as_bytes()).unwrap();
        let actions = run(tmp.path(), &mut state, &registry, &probes);
        assert_eq!(actions.len(), 1, "exactly the new line");
        assert_eq!(entry(&registry, ID).unwrap().state, SessionState::Working);
    }

    #[test]
    fn a_new_journal_appearing_while_running_is_replayed() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = SessionsRegistry::new();
        let mut state = JournalState::default();
        let probes = Fake::default();
        assert!(run(tmp.path(), &mut state, &registry, &probes).is_empty());
        write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::SessionStart,
                ago(1),
                "1-1",
                None,
            )],
        );
        run(tmp.path(), &mut state, &registry, &probes);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn an_event_whose_post_arrived_first_is_not_applied_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let t_work = ago(30);
        let t_stop = ago(20);
        let registry = SessionsRegistry::new();
        // The socket delivered both (and the Stop last).
        for (event, ts, seq) in [
            (SessionEvent::PreToolUse, t_work, "1-1"),
            (SessionEvent::Stop, t_stop, "1-2"),
        ] {
            registry.observe_stamped(
                rec(ID, Agent::Claude, event, ts, seq, None)
                    .to_observe_request()
                    .unwrap(),
                Some(EventStamp {
                    ts,
                    seq: seq.to_string(),
                }),
                Origin::Socket,
            );
        }
        assert_eq!(entry(&registry, ID).unwrap().state, SessionState::Idle);
        // The journal replays the same two: the working event must not undo the Stop.
        write(
            tmp.path(),
            &[
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::PreToolUse,
                    t_work,
                    "1-1",
                    None,
                ),
                rec(ID, Agent::Claude, SessionEvent::Stop, t_stop, "1-2", None),
            ],
        );
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        assert_eq!(entry(&registry, ID).unwrap().state, SessionState::Idle);
    }

    #[test]
    fn a_dropped_post_is_filled_in_without_undoing_a_newer_event() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = SessionsRegistry::new();
        // Event A's POST was dropped; event B (newer) got through.
        let b = ago(10);
        registry.observe_stamped(
            rec(ID, Agent::Claude, SessionEvent::Stop, b, "1-2", None)
                .to_observe_request()
                .unwrap(),
            Some(EventStamp {
                ts: b,
                seq: "1-2".to_string(),
            }),
            Origin::Socket,
        );
        write(
            tmp.path(),
            &[
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::PreToolUse,
                    ago(20),
                    "1-1",
                    None,
                ),
                rec(ID, Agent::Claude, SessionEvent::Stop, b, "1-2", None),
            ],
        );
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        assert_eq!(entry(&registry, ID).unwrap().state, SessionState::Idle);
    }

    #[test]
    fn records_that_do_not_belong_to_the_file_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(10),
                "1-1",
                None,
            )],
        );
        // A line for another session smuggled into this file, and one for the
        // wrong agent.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        for stray in [
            rec(
                ID2,
                Agent::Claude,
                SessionEvent::PreToolUse,
                ago(5),
                "9-1",
                None,
            ),
            rec(
                ID,
                Agent::Codex,
                SessionEvent::PreToolUse,
                ago(5),
                "9-2",
                None,
            ),
        ] {
            writeln!(f, "{}", serde_json::to_string(&stray).unwrap()).unwrap();
        }
        let registry = SessionsRegistry::new();
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        assert_eq!(registry.list().len(), 1);
        assert_eq!(entry(&registry, ID).unwrap().state, SessionState::Idle);
        assert!(entry(&registry, ID2).is_none());
    }

    #[test]
    fn notifications_replay_through_the_real_state_machine() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            &[
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::PreToolUse,
                    ago(40),
                    "1-1",
                    None,
                ),
                rec(
                    ID,
                    Agent::Claude,
                    SessionEvent::Notification(NotificationKind::PermissionPrompt),
                    ago(30),
                    "1-2",
                    None,
                ),
            ],
        );
        let registry = SessionsRegistry::new();
        run(
            tmp.path(),
            &mut JournalState::default(),
            &registry,
            &Fake::default(),
        );
        assert_eq!(
            entry(&registry, ID).unwrap().state,
            SessionState::WaitingForPermission
        );
    }

    // --- cleanup ------------------------------------------------------------

    fn age_file(path: &Path, secs: u64) {
        let when = SystemTime::now() - Duration::from_secs(secs);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    fn scan_at(dir: &Path, state: &mut JournalState, live: &[&str], probes: &Fake) -> Vec<Action> {
        let live: HashSet<String> = live.iter().map(|s| (*s).to_string()).collect();
        scan(dir, state, SystemTime::now(), &live, probes, &enrich)
    }

    #[test]
    fn a_journal_of_a_session_the_registry_dropped_is_deleted_once_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(30),
                "1-1",
                None,
            )],
        );
        let probes = Fake::default();
        let mut state = JournalState::default();
        // Imported this scan: never swept the same pass, however quiet the file.
        age_file(&path, 600);
        scan_at(tmp.path(), &mut state, &[], &probes);
        assert!(path.exists(), "protected the pass it is imported");
        // Next scan, the registry does not hold it live and the file is quiet.
        scan_at(tmp.path(), &mut state, &[], &probes);
        assert!(!path.exists());
        assert!(state.files.is_empty());
    }

    #[test]
    fn a_journal_of_a_live_session_is_kept_however_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(30),
                "1-1",
                None,
            )],
        );
        let probes = Fake::default();
        let mut state = JournalState::default();
        scan_at(tmp.path(), &mut state, &[ID], &probes);
        age_file(&path, 3600);
        scan_at(tmp.path(), &mut state, &[ID], &probes);
        assert!(path.exists());
    }

    #[test]
    fn a_recently_written_orphan_is_kept_through_the_grace_period() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(30),
                "1-1",
                None,
            )],
        );
        let probes = Fake::default();
        let mut state = JournalState::default();
        scan_at(tmp.path(), &mut state, &[], &probes);
        scan_at(tmp.path(), &mut state, &[], &probes);
        assert!(path.exists(), "an ended session's journal lingers a moment");
    }

    #[test]
    fn a_journal_older_than_the_cap_is_deleted_even_for_a_live_session() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(30),
                "1-1",
                None,
            )],
        );
        let probes = Fake::default();
        let mut state = JournalState::default();
        scan_at(tmp.path(), &mut state, &[ID], &probes);
        age_file(&path, 8 * 24 * 3600);
        scan_at(tmp.path(), &mut state, &[ID], &probes);
        assert!(!path.exists());
    }

    #[test]
    fn junk_and_foreign_files_are_left_alone_and_stale_temp_files_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let junk = agent_dir.join("notes.txt");
        let upper = agent_dir.join(format!("{}.jsonl", ID.to_uppercase()));
        let short = agent_dir.join("s1.jsonl");
        let fresh_tmp = agent_dir.join(format!(".{ID}.jsonl.tmp"));
        let stale_tmp = agent_dir.join(format!(".{ID2}.jsonl.tmp"));
        for p in [&junk, &upper, &short, &fresh_tmp, &stale_tmp] {
            std::fs::write(p, "x").unwrap();
        }
        age_file(&stale_tmp, 2 * 3600);
        let mut state = JournalState::default();
        scan_at(tmp.path(), &mut state, &[], &Fake::default());
        for kept in [&junk, &upper, &short, &fresh_tmp] {
            assert!(kept.exists(), "{kept:?}");
        }
        assert!(!stale_tmp.exists());
        assert!(state.files.is_empty());
    }

    #[test]
    fn a_symlink_is_never_followed_or_deleted_through() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "do not touch").unwrap();
        let agent_dir = tmp.path().join("sessions").join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let link = agent_dir.join(format!("{ID}.jsonl"));
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let mut state = JournalState::default();
        let actions = scan_at(
            &tmp.path().join("sessions"),
            &mut state,
            &[],
            &Fake::default(),
        );
        assert!(actions.is_empty());
        assert!(outside.exists() && link.symlink_metadata().is_ok());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not touch");
    }

    #[test]
    fn an_empty_journal_gets_a_scan_to_fill_in_and_is_then_swept_as_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let path = agent_dir.join(format!("{ID}.jsonl"));
        std::fs::write(&path, "").unwrap();
        let mut state = JournalState::default();
        scan_at(tmp.path(), &mut state, &[], &Fake::default());
        assert!(path.exists(), "a hook may be about to write it");
        age_file(&path, 600);
        scan_at(tmp.path(), &mut state, &[], &Fake::default());
        assert!(!path.exists());
    }

    // --- size cap -----------------------------------------------------------

    fn fill(dir: &Path, n: usize) -> PathBuf {
        // Recent enough for the TTL to accept the session, in order by `ts`.
        let base = ago(120);
        for i in 0..n {
            let record = rec(
                ID,
                Agent::Claude,
                SessionEvent::PreToolUse,
                base + chrono::Duration::milliseconds(i64::try_from(i).unwrap()),
                &format!("1-{i:05}"),
                None,
            );
            journal::append(dir, &record).unwrap();
        }
        journal::journal_path(dir, Agent::Claude, ID).unwrap()
    }

    #[test]
    fn an_oversized_journal_is_compacted_to_whole_tail_lines_and_stays_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = fill(tmp.path(), 900);
        let before = std::fs::metadata(&path).unwrap().len();
        assert!(before > MAX_JOURNAL_BYTES, "{before}");
        let mut state = JournalState::default();
        let registry = SessionsRegistry::new();
        let probes = Fake::default();
        run(tmp.path(), &mut state, &registry, &probes);

        let after = std::fs::metadata(&path).unwrap();
        assert!(
            after.len() <= COMPACT_TO_BYTES && after.len() > COMPACT_TO_BYTES / 2,
            "{}",
            after.len()
        );
        assert_eq!(after.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines().all(|l| JournalRecord::parse(l).is_some()),
            "whole lines only"
        );
        assert!(text.contains("1-00899"), "the newest records survive");
        assert!(!text.contains("1-00000"), "the oldest are gone");
        assert_eq!(state.files[&path].offset, after.len());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());

        // The next scan sees its own rewrite as already consumed, and an append
        // after it is tailed normally.
        assert!(run(tmp.path(), &mut state, &registry, &probes).is_empty());
        journal::append(
            tmp.path(),
            &rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(1),
                "1-99999",
                None,
            ),
        )
        .unwrap();
        assert_eq!(run(tmp.path(), &mut state, &registry, &probes).len(), 1);
        assert_eq!(entry(&registry, ID).unwrap().state, SessionState::Idle);
    }

    #[test]
    fn compaction_is_abandoned_when_the_file_changed_since_it_was_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = fill(tmp.path(), 900);
        let meta = std::fs::metadata(&path).unwrap();
        // The caller's view is stale: the length differs.
        assert_eq!(compact(&path, meta.len() - 10, meta.ino()).unwrap(), None);
        // ...or the inode does.
        assert_eq!(compact(&path, meta.len(), meta.ino() + 1).unwrap(), None);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            meta.len(),
            "untouched"
        );
        assert!(
            !path.with_file_name(format!(".{ID}.jsonl.tmp")).exists(),
            "an abandoned compaction leaves no temp file"
        );
    }

    #[test]
    fn a_journal_with_unread_bytes_is_not_compacted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = fill(tmp.path(), 900);
        let meta = std::fs::metadata(&path).unwrap();
        let file = JournalFile {
            path: path.clone(),
            id: ID.to_string(),
            len: meta.len(),
            ino: meta.ino(),
            mtime: SystemTime::now(),
        };
        let mut track = FileTrack {
            session_id: ID.to_string(),
            ino: meta.ino(),
            offset: meta.len() - 100,
            imported_in: 0,
        };
        compact_if_needed(&file, &mut track);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), meta.len());
    }

    #[test]
    fn a_rewritten_journal_is_re_read_without_reapplying_what_the_registry_has() {
        let tmp = tempfile::tempdir().unwrap();
        let path = fill(tmp.path(), 900);
        let registry = SessionsRegistry::new();
        let mut state = JournalState::default();
        let probes = Fake::default();
        run(tmp.path(), &mut state, &registry, &probes);
        // Something else rewrote the file (a new inode, shorter): re-read from 0.
        let text = std::fs::read_to_string(&path).unwrap();
        let mut last: Vec<&str> = text.lines().rev().take(5).collect();
        last.reverse();
        let tail = format!("{}\n", last.join("\n"));
        let replacement = path.with_extension("new");
        std::fs::write(&replacement, tail).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let actions = run(tmp.path(), &mut state, &registry, &probes);
        assert_eq!(actions.len(), 5, "re-read in full");
        assert_eq!(entry(&registry, ID).unwrap().state, SessionState::Working);
    }

    #[test]
    fn a_huge_backlog_is_read_from_its_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.jsonl");
        let line = format!(
            "{}\n",
            serde_json::to_string(&rec(
                ID,
                Agent::Claude,
                SessionEvent::Stop,
                ago(5),
                "1-1",
                None
            ))
            .unwrap()
        );
        let n = (MAX_READ_BYTES as usize / line.len()) + 100;
        std::fs::write(&path, line.repeat(n)).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let chunk = read_chunk(&path, 0, len).unwrap();
        assert!(chunk.records.len() < n && chunk.records.len() > n / 2);
        assert_eq!(chunk.new_offset, len);
    }

    // --- end to end ---------------------------------------------------------

    #[tokio::test]
    async fn the_spawned_watcher_replays_and_then_tails_into_the_registry() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            &[rec(
                ID,
                Agent::Claude,
                SessionEvent::UserPromptSubmit,
                ago(20),
                "1-1",
                None,
            )],
        );
        let registry = Arc::new(SessionsRegistry::new());
        let token = CancellationToken::new();
        let handle = spawn(
            registry.clone(),
            tmp.path().to_path_buf(),
            Arc::new(enrich),
            token.clone(),
        );
        let mut waited = 0;
        while registry.list().is_empty() && waited < 100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        let listed = registry.list();
        assert_eq!(listed.len(), 1, "the startup replay");
        assert_eq!(listed[0].state, SessionState::Working);
        assert_eq!(listed[0].source, Source::Terminal);
        token.cancel();
        handle.await.unwrap();
    }
}
