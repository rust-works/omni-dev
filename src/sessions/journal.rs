//! The durable per-session hook journals (#2108): the record format, the paths,
//! and the writer the hook sink uses.
//!
//! The registry is in memory, and the hook sink is fail-open: it gives up after
//! two seconds when the daemon is unreachable. So an event fired while the daemon
//! was restarting, or before a cold socket-activated daemon answered, used to be
//! lost for good, and a restart forgot every session. The sink now appends each
//! event to a journal **before** it posts it, and the daemon replays and tails
//! those journals ([`journal_watcher`](super::journal_watcher)), taking whatever
//! the socket missed.
//!
//! **Hooks own the events; the daemon owns the state.** A hook never computes a
//! state: [`SessionState::for_event`](super::SessionState::for_event) depends on
//! the current state, subagent waits roll up per session, a replaced process's
//! stragglers are filtered (#1948), and two hooks racing a read-modify-write
//! would lose one. So a journal holds raw events, one JSON object per line, and
//! the daemon runs them through the same state machine as a socket event.
//!
//! A record carries only what the sink already sends over the socket — ids, `cwd`,
//! the transcript *path*, the model id, the pid and the event kind — never a
//! prompt, a tool input or any transcript content.
//!
//! Layout, beside the control socket so the hook and the daemon resolve the same
//! directory by construction (a `--socket` override moves the journals with it):
//! `<socket dir>/sessions/<agent>/<session_id>.jsonl`, directories `0700`, files
//! `0600`. Writers append with `O_APPEND` single-line writes, so concurrent hook
//! processes need no lock. Cleanup is the daemon's.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use super::is_session_uuid;
use super::{Agent, EventStamp, ObserveRequest, SessionEvent};
use crate::daemon::paths;

/// How long a journal may sit unwritten before it is deleted, and the oldest last
/// event a replay will accept. Generous: a session left open and idle for days is
/// real, and a live pid is cheap proof.
pub const MAX_JOURNAL_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// The size past which the hook sink stops appending to a journal. The daemon
/// compacts a journal long before this, so reaching it means nothing is reading
/// or tending the journals (the sessions service is not running), and the sink
/// must not grow a file without bound on its own.
const SINK_MAX_BYTES: u64 = 512 * 1024;

/// The record format version. A reader skips a record with a newer version
/// rather than guess at its fields; unknown *fields* of a known version are
/// ignored, as everywhere on this wire.
pub const RECORD_VERSION: u32 = 1;

/// One journal line: a hook event, stamped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    /// The format version, [`RECORD_VERSION`].
    pub v: u32,
    /// When the hook fired (see [`EventStamp::ts`]).
    pub ts: DateTime<Utc>,
    /// The event's nonce (see [`EventStamp::seq`]).
    pub seq: String,
    /// Which agent's hook wrote it; also names the journal's directory.
    pub agent: Agent,
    /// The session the event belongs to; also names the journal file.
    pub session_id: String,
    /// What happened.
    #[serde(flatten)]
    pub body: JournalBody,
}

/// What a [`JournalRecord`] reports, tagged `op` on the wire like the socket ops
/// it mirrors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalBody {
    /// An `observe` sighting.
    Observe {
        /// The hook event, already mapped to a [`SessionEvent`] by the sink.
        event: SessionEvent,
        /// Claude subagent identity, for a subagent's hook.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// The session's working directory.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        /// The transcript's path, never its content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_path: Option<PathBuf>,
        /// The model id, when the payload carried one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The agent process's pid (the hook's parent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// A `SessionEnd`.
    End {
        /// Why the session ended, when the payload said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The agent process's pid, which tells a replaced process's late end
        /// from the owner's (#1948).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
}

impl JournalRecord {
    /// The record for an `observe` the sink is about to send.
    #[must_use]
    pub fn observe(req: &ObserveRequest, stamp: &EventStamp) -> Self {
        Self {
            v: RECORD_VERSION,
            ts: stamp.ts,
            seq: stamp.seq.clone(),
            agent: req.agent,
            session_id: req.session_id.clone(),
            body: JournalBody::Observe {
                event: req.event,
                agent_id: req.agent_id.clone(),
                cwd: req.cwd.clone(),
                transcript_path: req.transcript_path.clone(),
                model: req.model.clone(),
                pid: req.pid,
            },
        }
    }

    /// The record for a `SessionEnd` the sink is about to send.
    #[must_use]
    pub fn end(
        agent: Agent,
        session_id: &str,
        reason: Option<&str>,
        pid: Option<u32>,
        stamp: &EventStamp,
    ) -> Self {
        Self {
            v: RECORD_VERSION,
            ts: stamp.ts,
            seq: stamp.seq.clone(),
            agent,
            session_id: session_id.to_string(),
            body: JournalBody::End {
                reason: reason.map(str::to_string),
                pid,
            },
        }
    }

    /// This record's [`EventStamp`].
    #[must_use]
    pub fn stamp(&self) -> EventStamp {
        EventStamp {
            ts: self.ts,
            seq: self.seq.clone(),
        }
    }

    /// The pid the record names, if any.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        match &self.body {
            JournalBody::Observe { pid, .. } | JournalBody::End { pid, .. } => *pid,
        }
    }

    /// The `observe` this record replays as, or `None` for an `end`.
    #[must_use]
    pub fn to_observe_request(&self) -> Option<ObserveRequest> {
        let JournalBody::Observe {
            event,
            agent_id,
            cwd,
            transcript_path,
            model,
            pid,
        } = &self.body
        else {
            return None;
        };
        Some(ObserveRequest {
            agent_id: agent_id.clone(),
            session_id: self.session_id.clone(),
            cwd: cwd.clone(),
            transcript_path: transcript_path.clone(),
            event: *event,
            repo: None,
            model: model.clone(),
            agent: self.agent,
            pid: *pid,
        })
    }

    /// Parses one journal line. `None` for anything this reader should skip: a
    /// torn or garbled line, a record of a newer format version, or one naming
    /// an id or agent no journal could hold.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let record: Self = serde_json::from_str(line.trim()).ok()?;
        (record.v == RECORD_VERSION
            && is_session_uuid(&record.session_id)
            && agent_dir_name(record.agent).is_some())
        .then_some(record)
    }
}

/// A fresh stamp for an event firing now.
#[must_use]
pub fn new_stamp() -> EventStamp {
    let ts = Utc::now();
    EventStamp {
        seq: format!(
            "{}-{}",
            std::process::id(),
            ts.timestamp_nanos_opt().unwrap_or_default()
        ),
        ts,
    }
}

/// The directory name an agent's journals live under, or `None` for an agent
/// that has no hook feed (pi.dev reports through its own extension).
#[must_use]
pub fn agent_dir_name(agent: Agent) -> Option<&'static str> {
    match agent {
        Agent::Claude => Some("claude"),
        Agent::Codex => Some("codex"),
        Agent::Pi => None,
    }
}

/// The journal file for a session, under `dir` (the journal root), or `None`
/// when the agent has no journal or the id is not a UUID. The id is lower-cased,
/// so the same session never gets two files.
#[must_use]
pub fn journal_path(dir: &Path, agent: Agent, session_id: &str) -> Option<PathBuf> {
    let agent_dir = agent_dir_name(agent)?;
    is_session_uuid(session_id).then(|| {
        dir.join(agent_dir)
            .join(format!("{}.jsonl", session_id.to_ascii_lowercase()))
    })
}

/// Appends `record` to its session's journal under `dir`, creating the
/// directories `0700` and the file `0600` as needed.
///
/// One `write` of the whole line to an `O_APPEND` file, so concurrent hook
/// processes need no lock. The caller is the fail-open hook sink, which swallows
/// the error: a journal that cannot be written costs the durability of one event,
/// never a turn.
///
/// The sink cannot tell whether a daemon is reading, so it bounds itself: it does
/// not journal at all until the daemon's runtime directory (`dir`'s parent)
/// exists, stops appending to a file past [`SINK_MAX_BYTES`], and, whenever it
/// starts a new journal, deletes sibling journals untouched for
/// [`MAX_JOURNAL_AGE`]. A daemon-less install therefore cannot accumulate
/// journals without limit.
///
/// # Errors
///
/// When the session id or agent cannot be journaled, the runtime directory does
/// not exist, a directory or the file cannot be created or opened, the journal is
/// over its cap, or the write fails.
pub fn append(dir: &Path, record: &JournalRecord) -> Result<()> {
    let path = journal_path(dir, record.agent, &record.session_id)
        .context("the session id or agent cannot be journaled")?;
    let agent_dir = path
        .parent()
        .context("a journal path always has a parent")?;
    anyhow::ensure!(
        dir.parent().is_some_and(Path::is_dir),
        "the daemon's runtime directory does not exist yet"
    );
    paths::ensure_dir_0700(dir)?;
    paths::ensure_dir_0700(agent_dir)?;
    let mut line = serde_json::to_vec(record).context("failed to serialize the journal record")?;
    line.push(b'\n');
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to open journal {}", path.display()))?;
    paths::ensure_handle_0600(&file)?;
    let len = file.metadata().context("failed to stat the journal")?.len();
    anyhow::ensure!(len < SINK_MAX_BYTES, "the journal is over its size cap");
    if len == 0 {
        sweep_stale(agent_dir, &path);
    }
    (&file)
        .write_all(&line)
        .with_context(|| format!("failed to append to journal {}", path.display()))
}

/// Deletes the other `<uuid>.jsonl` journals in `agent_dir` that nothing has
/// written to for [`MAX_JOURNAL_AGE`]. Best-effort, and only regular files with a
/// journal's name; a symlink is never followed.
fn sweep_stale(agent_dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(agent_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_journal = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl"))
            .is_some_and(is_session_uuid);
        if path == keep || !is_journal {
            continue;
        }
        // `symlink_metadata`, so a link is never followed; an entry that vanished
        // since the listing is simply not stale.
        let stale = std::fs::symlink_metadata(&path).is_ok_and(|meta| {
            meta.is_file()
                && meta
                    .modified()
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|age| age > MAX_JOURNAL_AGE)
        });
        if stale {
            // Best-effort housekeeping: a failure only leaves the file for the
            // daemon's sweep.
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::debug!(%error, "session_journal_sweep_failed");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sessions::NotificationKind;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4c";

    fn stamp(ts: &str, seq: &str) -> EventStamp {
        EventStamp {
            ts: ts.parse().unwrap(),
            seq: seq.to_string(),
        }
    }

    fn observe_req(event: SessionEvent) -> ObserveRequest {
        ObserveRequest {
            agent_id: None,
            session_id: ID.to_string(),
            cwd: Some(PathBuf::from("/work/repo")),
            transcript_path: Some(PathBuf::from("/home/me/.claude/projects/x/t.jsonl")),
            event,
            repo: None,
            model: Some("claude-x".to_string()),
            agent: Agent::Claude,
            pid: Some(4242),
        }
    }

    #[test]
    fn an_observe_record_round_trips_through_a_line() {
        let st = stamp("2026-10-03T03:40:23.186123456Z", "7-1");
        let req = observe_req(SessionEvent::Notification(
            NotificationKind::PermissionPrompt,
        ));
        let record = JournalRecord::observe(&req, &st);
        let line = serde_json::to_string(&record).unwrap();
        assert!(!line.contains('\n'), "one record is one line");
        let parsed = JournalRecord::parse(&line).unwrap();
        assert_eq!(parsed, record);
        assert_eq!(parsed.stamp(), st);
        assert_eq!(parsed.pid(), Some(4242));
        let back = parsed.to_observe_request().unwrap();
        assert_eq!(back.session_id, ID);
        assert_eq!(back.event, req.event);
        assert_eq!(back.cwd, req.cwd);
        assert_eq!(back.pid, Some(4242));
        assert_eq!(back.repo, None, "repo is the daemon's to enrich");
    }

    #[test]
    fn the_line_shape_is_the_documented_one() {
        let st = stamp("2026-10-03T03:40:23.186Z", "7-1");
        let mut req = observe_req(SessionEvent::PostToolUse);
        req.agent = Agent::Codex;
        req.transcript_path = None;
        req.model = None;
        let value: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&JournalRecord::observe(&req, &st)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "v": 1,
                "ts": "2026-10-03T03:40:23.186Z",
                "seq": "7-1",
                "agent": "codex",
                "session_id": ID,
                "op": "observe",
                "event": "post_tool_use",
                "cwd": "/work/repo",
                "pid": 4242,
            })
        );
    }

    #[test]
    fn an_end_record_round_trips_and_is_not_an_observe() {
        let st = stamp("2026-10-03T03:41:00Z", "7-2");
        let record = JournalRecord::end(Agent::Claude, ID, Some("clear"), Some(9), &st);
        let parsed = JournalRecord::parse(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(parsed, record);
        assert_eq!(parsed.pid(), Some(9));
        assert!(parsed.to_observe_request().is_none());
    }

    #[test]
    fn parse_skips_junk_newer_versions_and_unjournalable_ids() {
        let st = stamp("2026-10-03T03:41:00Z", "7-2");
        let good = JournalRecord::observe(&observe_req(SessionEvent::Stop), &st);
        let line = serde_json::to_string(&good).unwrap();
        assert!(JournalRecord::parse(&line).is_some());
        // A torn line (the writer was killed mid-write) and plain junk.
        assert!(JournalRecord::parse(&line[..line.len() / 2]).is_none());
        assert!(JournalRecord::parse("not json").is_none());
        assert!(JournalRecord::parse("").is_none());
        // A format this reader does not know.
        let newer = line.replacen("\"v\":1", "\"v\":2", 1);
        assert!(JournalRecord::parse(&newer).is_none());
        // An id no journal is named for, and an agent with no journal.
        let odd_id = line.replacen(ID, "../../etc/passwd", 1);
        assert!(JournalRecord::parse(&odd_id).is_none());
        let pi = line.replacen("\"agent\":\"claude\"", "\"agent\":\"pi\"", 1);
        assert!(JournalRecord::parse(&pi).is_none());
        // An unknown extra field is ignored.
        let extra = line.replacen("{\"v\":1", "{\"future\":true,\"v\":1", 1);
        assert!(JournalRecord::parse(&extra).is_some());
    }

    #[test]
    fn only_canonical_uuids_name_a_journal() {
        assert!(is_session_uuid(ID));
        assert!(is_session_uuid(&ID.to_uppercase()));
        for bad in [
            "",
            "s1",
            "..",
            "../x",
            "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4", // one short
            "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4cc", // one long
            "0b7e6c1a2f4d4a8e9c3b5d1e7f9a2b4c",    // no hyphens
            "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4g", // not hex
            "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4/",
            "0b7e6c1a-2f4d-4a8e-9c3b-5d1e7f9a2b4c\n",
        ] {
            assert!(!is_session_uuid(bad), "{bad:?}");
        }
        let dir = Path::new("/j");
        assert_eq!(
            journal_path(dir, Agent::Claude, ID),
            Some(PathBuf::from(format!("/j/claude/{ID}.jsonl")))
        );
        assert_eq!(
            journal_path(dir, Agent::Codex, &ID.to_uppercase()),
            Some(PathBuf::from(format!("/j/codex/{ID}.jsonl"))),
            "the same session maps to one file whatever the case"
        );
        assert_eq!(journal_path(dir, Agent::Pi, ID), None);
        assert_eq!(journal_path(dir, Agent::Claude, "../../x"), None);
    }

    #[test]
    fn new_stamps_are_unique_per_event() {
        let a = new_stamp();
        let b = new_stamp();
        assert_ne!(a.seq, b.seq);
        assert!(a.seq.starts_with(&format!("{}-", std::process::id())));
    }

    #[test]
    fn append_creates_a_private_tree_and_adds_whole_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let first = JournalRecord::observe(
            &observe_req(SessionEvent::SessionStart),
            &stamp("2026-10-03T03:40:00Z", "7-1"),
        );
        let second = JournalRecord::end(
            Agent::Claude,
            ID,
            None,
            Some(4242),
            &stamp("2026-10-03T03:41:00Z", "7-2"),
        );
        append(&dir, &first).unwrap();
        append(&dir, &second).unwrap();

        let path = journal_path(&dir, Agent::Claude, ID).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<JournalRecord> = text
            .lines()
            .map(|l| JournalRecord::parse(l).unwrap())
            .collect();
        assert_eq!(records, vec![first, second]);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn append_tightens_a_looser_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let path = journal_path(&dir, Agent::Claude, ID).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append(
            &dir,
            &JournalRecord::observe(
                &observe_req(SessionEvent::Stop),
                &stamp("2026-10-03T03:40:00Z", "7-1"),
            ),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn append_refuses_an_unjournalable_session_and_reports_an_unwritable_root() {
        let tmp = tempfile::tempdir().unwrap();
        let st = stamp("2026-10-03T03:40:00Z", "7-1");
        let mut bad = observe_req(SessionEvent::Stop);
        bad.session_id = "../../escape".to_string();
        assert!(append(tmp.path(), &JournalRecord::observe(&bad, &st)).is_err());
        assert!(
            std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
            "a refused id must not create anything"
        );
        // The root is a file: the directory cannot be made, so it errors
        // (the sink swallows it).
        let file_root = tmp.path().join("sessions");
        std::fs::write(&file_root, "").unwrap();
        let ok = observe_req(SessionEvent::Stop);
        assert!(append(&file_root, &JournalRecord::observe(&ok, &st)).is_err());
    }

    #[test]
    fn concurrent_appenders_never_split_a_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let record = JournalRecord::observe(
                            &observe_req(SessionEvent::PreToolUse),
                            &stamp("2026-10-03T03:40:00Z", &format!("{t}-{i}")),
                        );
                        append(&dir, &record).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = std::fs::read_to_string(journal_path(&dir, Agent::Claude, ID).unwrap()).unwrap();
        assert_eq!(text.lines().count(), 400);
        assert!(text.lines().all(|l| JournalRecord::parse(l).is_some()));
    }

    #[test]
    fn a_record_never_carries_conversation_content() {
        // The sink's payload has prompts and tool inputs; `ObserveRequest` and so
        // the record have no field for them. Pin that the line holds only the
        // documented keys.
        let req = observe_req(SessionEvent::UserPromptSubmit);
        let value = serde_json::to_value(JournalRecord::observe(
            &req,
            &stamp("2026-10-03T03:40:00Z", "7-1"),
        ))
        .unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "agent",
                "cwd",
                "event",
                "model",
                "op",
                "pid",
                "seq",
                "session_id",
                "transcript_path",
                "ts",
                "v"
            ]
        );
    }

    fn age_file(path: &Path, secs: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn the_sink_does_not_journal_until_the_daemons_runtime_directory_exists() {
        // The journal root is `<runtime dir>/sessions`: a machine that has never
        // run the daemon has no runtime dir, and the hook must not create one.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("never-ran").join("sessions");
        let record = JournalRecord::observe(
            &observe_req(SessionEvent::Stop),
            &stamp("2026-10-03T03:40:00Z", "7-1"),
        );
        let err = append(&dir, &record).unwrap_err();
        assert!(err.to_string().contains("runtime directory"), "{err}");
        assert!(!tmp.path().join("never-ran").exists());
    }

    #[test]
    fn the_sink_stops_appending_to_a_journal_nothing_is_tending() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let record = JournalRecord::observe(
            &observe_req(SessionEvent::Stop),
            &stamp("2026-10-03T03:40:00Z", "7-1"),
        );
        append(&dir, &record).unwrap();
        let path = journal_path(&dir, Agent::Claude, ID).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(SINK_MAX_BYTES)
            .unwrap();
        let err = append(&dir, &record).unwrap_err();
        assert!(err.to_string().contains("size cap"), "{err}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), SINK_MAX_BYTES);
    }

    #[test]
    fn starting_a_journal_sweeps_week_old_siblings_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let agent_dir = dir.join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let stale = agent_dir.join("1c8f7d2b-3a5e-4b9f-8d4c-6e2f8a0b3c5d.jsonl");
        let fresh = agent_dir.join("2d9a8e3c-4b6f-4c0a-9e5d-7f3a9b1c4d6e.jsonl");
        let foreign = agent_dir.join("notes.jsonl");
        for p in [&stale, &fresh, &foreign] {
            std::fs::write(p, "x\n").unwrap();
        }
        age_file(&stale, 8 * 24 * 3600);
        age_file(&foreign, 8 * 24 * 3600);
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "keep").unwrap();
        let link = agent_dir.join("3e0b9f4d-5c7a-4d1b-8f6e-8a4b0c2d5e7f.jsonl");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let record = JournalRecord::observe(
            &observe_req(SessionEvent::SessionStart),
            &stamp("2026-10-03T03:40:00Z", "7-1"),
        );
        append(&dir, &record).unwrap();
        assert!(!stale.exists(), "a week-old journal is swept");
        assert!(
            fresh.exists() && foreign.exists(),
            "only stale journals by name"
        );
        assert!(
            link.symlink_metadata().is_ok() && outside.exists(),
            "no symlink followed"
        );

        // An append to an existing journal does not sweep again.
        age_file(&fresh, 8 * 24 * 3600);
        append(&dir, &record).unwrap();
        assert!(fresh.exists());
    }

    #[test]
    fn sweeping_a_directory_that_is_not_there_does_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("claude");
        sweep_stale(&missing, &missing.join(format!("{ID}.jsonl")));
        assert!(!missing.exists());
    }

    #[test]
    fn a_stale_journal_that_cannot_be_deleted_is_left_for_the_daemons_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let stale = agent_dir.join("1c8f7d2b-3a5e-4b9f-8d4c-6e2f8a0b3c5d.jsonl");
        let keep = agent_dir.join("2d9a8e3c-4b6f-4c0a-9e5d-7f3a9b1c4d6e.jsonl");
        std::fs::write(&stale, "x\n").unwrap();
        age_file(&stale, 8 * 24 * 3600);

        // A directory nobody may write to refuses the unlink.
        std::fs::set_permissions(&agent_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Root ignores directory permissions, so only then is the unlink allowed.
        let privileged = std::fs::write(agent_dir.join("probe"), "").is_ok();
        sweep_stale(&agent_dir, &keep);
        std::fs::set_permissions(&agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(stale.exists(), !privileged, "privileged: {privileged}");
    }
}
