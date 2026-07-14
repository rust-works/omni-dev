//! The cross-window Claude Code session registry engine.
//!
//! Maintains the live, authoritative set of running Claude Code sessions across
//! *every* terminal and VS Code window for the logged-in user, with a coarse
//! inferred state (working / idle / waiting-for-input / waiting-for-permission).
//! Fed by three independent feeds that each degrade gracefully — Claude Code
//! **hooks** (`omni-dev sessions hook`), a **transcript-file watcher** over
//! `~/.claude/projects/**/*.jsonl`, and the companion VS Code extension
//! reporting each window's embedded Claude tabs/terminals. See ADR-0052.
//!
//! This is the standalone engine, analogous to [`crate::worktrees`],
//! [`crate::browser`], and [`crate::snowflake`]; the daemon adapter lives in
//! [`crate::daemon::services::sessions`].
//!
//! Like the worktrees engine this is cheap and in-memory — no async setup, no
//! secret persisted. Two maps live behind a pair of [`std::sync::Mutex`]es that
//! are **never held across an `.await`** (the Snowflake rule): the *sessions*
//! keyed by their Claude `session_id`, and the *windows* keyed by the companion's
//! per-window key (the Claude-embedding reports used to tag a session's source).
//! Every op is pure CPU under a lock, so liveness reaping happens inline on each
//! read rather than from a background task — exactly as [`crate::worktrees`] does.
//!
//! State is **inferred**, not first-class: Claude Code exposes no dedicated
//! session-state event, so `working`/`idle` is best-effort (see
//! [`SessionState::for_event`]). `waiting_for_permission` / `waiting_for_input`
//! are reliable (they come from a `Notification` hook); the transcript watcher
//! backstops the "thinking window" where no hook fires.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod watcher;

/// How long a session may go silent before it ages out of the registry.
///
/// Unlike a VS Code window (which heartbeats every ~10s), a running Claude
/// session emits nothing while idle at the prompt, so its only liveness signal
/// is activity — a hook event or transcript growth. The TTL is therefore
/// generous: a session that has done nothing for this long is assumed gone (a
/// `claude` that exited without firing `SessionEnd`) and reaped on the next read.
/// A still-alive idle session re-appears the moment it next does anything. This
/// is the accepted limitation of the hook-based approach — see ADR-0052.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(300);

/// How long an **ended** session lingers before it is reaped, so `sessions list`
/// briefly shows a session that just finished (`SessionEnd` fired → [`end`]) as
/// `ended` rather than having it vanish instantly.
///
/// [`end`]: SessionsRegistry::end
const ENDED_SESSION_TTL: Duration = Duration::from_secs(10);

/// How long a companion window-embedding report survives without a refresh.
/// Mirrors the worktrees window TTL (three missed ~10s heartbeats): a window
/// that crashed without unregistering stops tagging its sessions as VS Code
/// embedded on the next read.
const DEFAULT_WINDOW_TTL: Duration = Duration::from_secs(30);

/// Ceiling on live session entries, so a runaway feed cannot grow daemon memory
/// faster than the TTL reaps it (the worktrees `MAX_WINDOWS` precedent, #1140).
/// Far above any real concurrent-session count; at the cap a genuinely new
/// session evicts the longest-silent entry rather than being rejected, so ingest
/// stays infallible.
const MAX_SESSIONS: usize = 512;

/// Ceiling on live window-embedding reports, mirroring the worktrees registry cap.
const MAX_WINDOWS: usize = 256;

/// The coarse, inferred lifecycle state of a Claude Code session.
///
/// Serialized `snake_case` (`waiting_for_permission`, …) into `list`/`status`
/// payloads. `waiting_for_*` are **reliable** (a `Notification` hook fires them
/// directly); `working`/`idle` are best-effort inference from `PreToolUse` /
/// `Stop` plus the transcript-growth backstop (Claude Code ships no dedicated
/// state event — ADR-0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Session just started (`SessionStart`), before any turn.
    Starting,
    /// Actively processing a turn — a tool call (`PreToolUse`/`PostToolUse`), a
    /// submitted prompt (`UserPromptSubmit`), or observed transcript growth.
    Working,
    /// Finished a turn and waiting at the prompt (`Stop`).
    Idle,
    /// Blocked on the user for a plain input/idle notification.
    WaitingForInput,
    /// Blocked on the user to approve a tool/permission prompt.
    WaitingForPermission,
    /// The session ended (`SessionEnd`); reaped shortly after via
    /// [`ENDED_SESSION_TTL`].
    Ended,
}

impl SessionState {
    /// The state a sighting of `event` implies, given the session's `current`
    /// state (`None` for a brand-new session). This is the whole inference
    /// machine, kept in one testable place:
    ///
    /// - `SessionStart` → [`Starting`](Self::Starting)
    /// - `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `TranscriptGrew` →
    ///   [`Working`](Self::Working)
    /// - `Stop` → [`Idle`](Self::Idle)
    /// - `Notification(PermissionPrompt)` →
    ///   [`WaitingForPermission`](Self::WaitingForPermission)
    /// - `Notification(IdlePrompt | AgentNeedsInput)` →
    ///   [`WaitingForInput`](Self::WaitingForInput)
    /// - `Notification(Other)` → **unchanged** (an unclassified notification is
    ///   not evidence of a state change)
    /// - `TranscriptDiscovered` → the current state if known, else
    ///   [`Idle`](Self::Idle) (a passively-discovered session's activity is
    ///   unknown; a later hook or growth upgrades it)
    #[must_use]
    pub fn for_event(event: &SessionEvent, current: Option<Self>) -> Self {
        match event {
            SessionEvent::SessionStart => Self::Starting,
            SessionEvent::UserPromptSubmit
            | SessionEvent::PreToolUse
            | SessionEvent::PostToolUse
            | SessionEvent::TranscriptGrew => Self::Working,
            SessionEvent::Stop => Self::Idle,
            SessionEvent::Notification(NotificationKind::PermissionPrompt) => {
                Self::WaitingForPermission
            }
            SessionEvent::Notification(
                NotificationKind::IdlePrompt | NotificationKind::AgentNeedsInput,
            ) => Self::WaitingForInput,
            // An unclassified notification carries no state signal, and a
            // passively-discovered transcript's activity is unknown: keep the
            // current state (or default a brand-new session to Idle).
            SessionEvent::Notification(NotificationKind::Other)
            | SessionEvent::TranscriptDiscovered => current.unwrap_or(Self::Idle),
        }
    }
}

/// The classification of a Claude Code `Notification` hook.
///
/// Derived by the hook sink from the notification message (the message text is
/// version-unstable, so classification is best-effort with an
/// [`Other`](Self::Other) fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// Claude is asking to run a tool / use a permission — reliably
    /// [`WaitingForPermission`](SessionState::WaitingForPermission).
    PermissionPrompt,
    /// Claude has been idle waiting for the user to respond.
    IdlePrompt,
    /// An agent/subagent needs the user's input.
    AgentNeedsInput,
    /// A notification we could not classify — carries no state signal.
    Other,
}

/// A sighting of a session, from a hook event or the transcript watcher.
///
/// Drives the [`SessionState::for_event`] inference and refreshes liveness.
/// Serialized on the wire as part of an [`ObserveRequest`]; `snake_case`, with
/// the notification kind nested (`{"notification":"permission_prompt"}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEvent {
    /// `SessionStart` hook.
    SessionStart,
    /// `UserPromptSubmit` hook — a prompt was submitted.
    UserPromptSubmit,
    /// `PreToolUse` hook — about to run a tool.
    PreToolUse,
    /// `PostToolUse` hook — a tool finished.
    PostToolUse,
    /// `Stop` hook — the turn finished.
    Stop,
    /// `Notification` hook, classified into a [`NotificationKind`].
    Notification(NotificationKind),
    /// The transcript watcher saw this session's `.jsonl` grow (the
    /// "thinking-window" backstop, where no hook fires).
    TranscriptGrew,
    /// The transcript watcher discovered a session's `.jsonl` it had not seen —
    /// a session that started before the daemon, or before hooks were installed.
    TranscriptDiscovered,
}

/// Where a session is running, resolved at [`list`](SessionsRegistry::list) time
/// by joining a session's `cwd` against the companion's window-embedding reports.
///
/// A session whose `cwd` lies under a reporting VS Code window that has ≥1 Claude
/// tab/terminal is tagged [`VsCode`](Self::VsCode); everything else is
/// [`Terminal`](Self::Terminal) — meaning "not matched to a reporting VS Code
/// window" (a bare terminal session, or a VS Code session whose companion is not
/// installed). Serialized as `{"kind":"terminal"}` /
/// `{"kind":"vs_code","window_key":"…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Not matched to any reporting VS Code window.
    Terminal,
    /// Embedded in a VS Code window (matched by `cwd`), carrying that window's
    /// companion key for a focus action.
    VsCode {
        /// The matched window's companion key.
        window_key: String,
    },
}

/// An idempotent session sighting sent to the registry — the wire payload of the
/// `observe` op, and the argument to [`SessionsRegistry::observe`].
///
/// The hook sink and the transcript watcher both produce these; every field but
/// `session_id` and `event` is best-effort and *fills in* missing data on an
/// existing entry without ever clobbering known data with `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveRequest {
    /// The Claude `session_id` (a UUID) — the primary key. Equal to the
    /// transcript filename stem and (per ADR-0052) the VS Code extension's tab
    /// key, so the three feeds join without heuristics.
    pub session_id: String,
    /// The session's working directory, when known (from the hook `cwd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The `~/.claude/projects/**/<session-id>.jsonl` transcript path, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    /// The event that produced this sighting; drives the state inference.
    pub event: SessionEvent,
    /// The repository name enriched from `cwd` by the adapter (git2), when
    /// resolvable. Stored verbatim; the engine does no disk I/O.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The model id, when a hook reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// A companion report of one VS Code window's embedded Claude sessions.
///
/// The wire payload of the `window` op. The companion cannot expose a tab's
/// `session_id` (Claude Code's extension has no public API — ADR-0052), so it
/// reports only the *counts* of Claude tabs/terminals plus the window's folders;
/// the join to a specific session is by `cwd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowReport {
    /// The companion-owned per-window key (also the worktrees registration key).
    pub key: String,
    /// The window's workspace-folder absolute paths, for the `cwd` join.
    #[serde(default)]
    pub folders: Vec<PathBuf>,
    /// How many Claude editor tabs (`claudeVSCodePanel` webviews) the window has.
    #[serde(default)]
    pub tabs: usize,
    /// How many Claude Code integrated terminals the window has.
    #[serde(default)]
    pub terminals: usize,
}

impl WindowReport {
    /// Whether this window has any Claude embedding at all — the gate for
    /// tagging a matching session as [`Source::VsCode`].
    #[must_use]
    fn has_embedding(&self) -> bool {
        self.tabs > 0 || self.terminals > 0
    }
}

/// One live session in the registry.
///
/// Serialized verbatim into `list` / `status` payloads; consumers compute age
/// from `last_seen` (RFC 3339). `source` is resolved at
/// [`list`](SessionsRegistry::list) time (stored as [`Source::Terminal`] until
/// then).
#[derive(Debug, Clone, Serialize)]
pub struct SessionEntry {
    /// The Claude `session_id`.
    pub session_id: String,
    /// The session's working directory, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The transcript path, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    /// The repository name enriched from `cwd`, when resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The model id, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The current inferred state.
    pub state: SessionState,
    /// Where the session runs, resolved on read.
    pub source: Source,
    /// The most recent event observed for this session.
    pub last_event: SessionEvent,
    /// When the session was first observed (RFC 3339).
    pub started_at: DateTime<Utc>,
    /// When the registry last heard from this session (RFC 3339).
    pub last_seen: DateTime<Utc>,
}

/// One companion window-embedding report, with its liveness stamp.
#[derive(Debug, Clone)]
struct WindowEntry {
    /// The report as sent by the companion.
    report: WindowReport,
    /// When the report last arrived (register or refresh).
    last_seen: DateTime<Utc>,
}

/// The cross-window session registry.
///
/// The in-memory, TTL-reaped set of running Claude sessions plus the companion
/// window-embedding reports used to tag a session's [`Source`]. Hosted by
/// [`SessionsService`](crate::daemon::services::sessions::SessionsService).
pub struct SessionsRegistry {
    /// Live sessions keyed by `session_id`.
    sessions: Mutex<HashMap<String, SessionEntry>>,
    /// Companion window-embedding reports keyed by window key. Behind its own
    /// mutex, taken independently of `sessions`, so the two never nest.
    windows: Mutex<HashMap<String, WindowEntry>>,
    /// How long a session survives without activity.
    session_ttl: Duration,
    /// How long an `ended` session lingers before reaping.
    ended_ttl: Duration,
    /// How long a window-embedding report survives without a refresh.
    window_ttl: Duration,
}

impl SessionsRegistry {
    /// Creates the registry with the default liveness TTLs. Cheap — no I/O.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            windows: Mutex::new(HashMap::new()),
            session_ttl: DEFAULT_SESSION_TTL,
            ended_ttl: ENDED_SESSION_TTL,
            window_ttl: DEFAULT_WINDOW_TTL,
        }
    }

    /// Locks the sessions map, recovering from a poisoned mutex (a panic in a
    /// prior critical section must not wedge the whole registry).
    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Locks the windows map, recovering from a poisoned mutex.
    fn lock_windows(&self) -> MutexGuard<'_, HashMap<String, WindowEntry>> {
        self.windows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records (upserts) a session sighting, running the [`SessionState`]
    /// inference and refreshing liveness. Reaps stale entries first, then — only
    /// when a genuinely new session would grow the map past [`MAX_SESSIONS`] —
    /// evicts the longest-silent entry. Infallible: an upsert never evicts.
    ///
    /// Best-effort fields (`cwd`/`transcript_path`/`repo`/`model`) *fill in* on
    /// an existing entry and never overwrite known data with `None`, so a later
    /// hook enriches a watcher-discovered session without a race losing data.
    pub fn observe(&self, req: ObserveRequest) {
        let now = Utc::now();
        let mut sessions = self.lock_sessions();
        reap_sessions(&mut sessions, self.session_ttl, self.ended_ttl, now);
        if let Some(entry) = sessions.get_mut(&req.session_id) {
            entry.state = SessionState::for_event(&req.event, Some(entry.state));
            entry.last_event = req.event;
            entry.last_seen = now;
            fill(&mut entry.cwd, req.cwd);
            fill(&mut entry.transcript_path, req.transcript_path);
            fill(&mut entry.repo, req.repo);
            fill(&mut entry.model, req.model);
        } else {
            if sessions.len() >= MAX_SESSIONS {
                evict_oldest_session(&mut sessions);
            }
            let state = SessionState::for_event(&req.event, None);
            sessions.insert(
                req.session_id.clone(),
                SessionEntry {
                    session_id: req.session_id,
                    cwd: req.cwd,
                    transcript_path: req.transcript_path,
                    repo: req.repo,
                    model: req.model,
                    state,
                    source: Source::Terminal,
                    last_event: req.event,
                    started_at: now,
                    last_seen: now,
                },
            );
        }
    }

    /// Marks a session ended (`SessionEnd`), so `list` shows it as `ended` for a
    /// short window ([`ENDED_SESSION_TTL`]) before it is reaped. Returns whether
    /// the session was known. A no-op for an already-unknown session (a
    /// duplicate/late `SessionEnd`).
    pub fn end(&self, session_id: &str, _reason: Option<&str>) -> bool {
        let now = Utc::now();
        let mut sessions = self.lock_sessions();
        reap_sessions(&mut sessions, self.session_ttl, self.ended_ttl, now);
        match sessions.get_mut(session_id) {
            Some(entry) => {
                entry.state = SessionState::Ended;
                entry.last_event = SessionEvent::Stop;
                entry.last_seen = now;
                true
            }
            None => false,
        }
    }

    /// Records (upserts) a companion window-embedding report and refreshes its
    /// liveness. Reaps stale windows first, then caps like [`observe`](Self::observe).
    pub fn report_window(&self, report: WindowReport) {
        let now = Utc::now();
        let mut windows = self.lock_windows();
        reap_windows(&mut windows, self.window_ttl, now);
        if !windows.contains_key(&report.key) && windows.len() >= MAX_WINDOWS {
            evict_oldest_window(&mut windows);
        }
        windows.insert(
            report.key.clone(),
            WindowEntry {
                report,
                last_seen: now,
            },
        );
    }

    /// Drops a companion window-embedding report (the window closed). Returns
    /// whether an entry was present.
    pub fn unregister_window(&self, key: &str) -> bool {
        let mut windows = self.lock_windows();
        windows.remove(key).is_some()
    }

    /// Reaps stale sessions and windows, then returns the live sessions with
    /// each [`Source`] resolved and sorted for deterministic output.
    ///
    /// Two independent locks, each held only for pure-CPU work and never
    /// nested: the sessions snapshot is taken and the lock dropped, then the
    /// windows snapshot, then the join runs lock-free. Path matching is a pure
    /// prefix compare (no canonicalization / disk I/O), honouring the
    /// `Mutex`-never-across-`.await` and no-I/O-under-lock invariants.
    pub fn list(&self) -> Vec<SessionEntry> {
        let now = Utc::now();
        let mut sessions: Vec<SessionEntry> = {
            let mut guard = self.lock_sessions();
            reap_sessions(&mut guard, self.session_ttl, self.ended_ttl, now);
            guard.values().cloned().collect()
        };
        let windows: Vec<WindowReport> = {
            let mut guard = self.lock_windows();
            reap_windows(&mut guard, self.window_ttl, now);
            guard
                .values()
                .map(|e| e.report.clone())
                .filter(WindowReport::has_embedding)
                .collect()
        };
        for session in &mut sessions {
            session.source = resolve_source(session.cwd.as_deref(), &windows);
        }
        sessions.sort_by(|a, b| {
            a.repo
                .cmp(&b.repo)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        sessions
    }

    /// The first workspace folder of the still-live window a session is embedded
    /// in, if any — used by the tray "focus" action to resolve a session to a
    /// folder to open in VS Code. `None` when the session has no `cwd`, or is not
    /// matched to a reporting window with a folder.
    pub fn focus_folder(&self, session_id: &str) -> Option<PathBuf> {
        let cwd = {
            let sessions = self.lock_sessions();
            sessions.get(session_id).and_then(|e| e.cwd.clone())
        }?;
        let now = Utc::now();
        let mut windows = self.lock_windows();
        reap_windows(&mut windows, self.window_ttl, now);
        windows
            .values()
            .map(|e| &e.report)
            .filter(|w| w.has_embedding())
            .filter(|w| w.folders.iter().any(|f| cwd.starts_with(f)))
            .find_map(|w| w.folders.first().cloned())
    }
}

impl Default for SessionsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Fills `slot` from `incoming` only when `incoming` carries a value, so a
/// best-effort field never overwrites known data with `None` on a re-`observe`.
fn fill<T>(slot: &mut Option<T>, incoming: Option<T>) {
    if let Some(value) = incoming {
        *slot = Some(value);
    }
}

/// Resolves a session's [`Source`] by joining its `cwd` against the live
/// window-embedding reports.
///
/// Among the windows whose folder is a prefix of `cwd`, the one with the lowest
/// key wins (a deterministic tiebreak). A session with no `cwd`, or no matching
/// window, is [`Source::Terminal`].
fn resolve_source(cwd: Option<&Path>, windows: &[WindowReport]) -> Source {
    let Some(cwd) = cwd else {
        return Source::Terminal;
    };
    let matched = windows
        .iter()
        .filter(|w| w.folders.iter().any(|f| cwd.starts_with(f)))
        .min_by(|a, b| a.key.cmp(&b.key));
    match matched {
        Some(window) => Source::VsCode {
            window_key: window.key.clone(),
        },
        None => Source::Terminal,
    }
}

/// Removes sessions last seen longer than their TTL ago (a shorter
/// [`ended_ttl`](SessionsRegistry::ended_ttl) for `ended` sessions), returning
/// how many were dropped. Pure CPU; the caller holds the sessions lock but never
/// `.await`s under it.
fn reap_sessions(
    sessions: &mut HashMap<String, SessionEntry>,
    session_ttl: Duration,
    ended_ttl: Duration,
    now: DateTime<Utc>,
) -> usize {
    let session_max = session_ttl.as_secs() as i64;
    let ended_max = ended_ttl.as_secs() as i64;
    let before = sessions.len();
    sessions.retain(|_, e| {
        let max_age = if e.state == SessionState::Ended {
            ended_max
        } else {
            session_max
        };
        (now - e.last_seen).num_seconds() <= max_age
    });
    before - sessions.len()
}

/// Removes window-embedding reports last refreshed longer than `ttl` ago.
fn reap_windows(
    windows: &mut HashMap<String, WindowEntry>,
    ttl: Duration,
    now: DateTime<Utc>,
) -> usize {
    let max_age = ttl.as_secs() as i64;
    let before = windows.len();
    windows.retain(|_, e| (now - e.last_seen).num_seconds() <= max_age);
    before - windows.len()
}

/// Removes the session with the oldest `last_seen` (ties broken by lowest
/// `session_id` for determinism). Called when a new session would exceed
/// [`MAX_SESSIONS`].
fn evict_oldest_session(sessions: &mut HashMap<String, SessionEntry>) {
    let oldest = sessions
        .values()
        .min_by(|a, b| {
            a.last_seen
                .cmp(&b.last_seen)
                .then_with(|| a.session_id.cmp(&b.session_id))
        })
        .map(|e| e.session_id.clone());
    if let Some(key) = oldest {
        sessions.remove(&key);
    }
}

/// Removes the window report with the oldest `last_seen` (ties broken by lowest
/// key). Called when a new window would exceed [`MAX_WINDOWS`].
fn evict_oldest_window(windows: &mut HashMap<String, WindowEntry>) {
    let oldest = windows
        .iter()
        .min_by(|a, b| a.1.last_seen.cmp(&b.1.last_seen).then_with(|| a.0.cmp(b.0)))
        .map(|(k, _)| k.clone());
    if let Some(key) = oldest {
        windows.remove(&key);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn observe_request(session_id: &str, event: SessionEvent, cwd: Option<&str>) -> ObserveRequest {
        ObserveRequest {
            session_id: session_id.to_string(),
            cwd: cwd.map(PathBuf::from),
            transcript_path: None,
            event,
            repo: None,
            model: None,
        }
    }

    #[test]
    fn list_is_empty_initially() {
        let reg = SessionsRegistry::new();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn observe_then_list_round_trips_and_infers_state() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::SessionStart,
            Some("/tmp/a"),
        ));
        let sessions = reg.list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s1");
        assert_eq!(sessions[0].state, SessionState::Starting);
        // No window reports → a bare terminal session.
        assert_eq!(sessions[0].source, Source::Terminal);
    }

    #[test]
    fn observe_is_idempotent_upsert_advancing_state() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::SessionStart,
            Some("/tmp/a"),
        ));
        reg.observe(observe_request("s1", SessionEvent::PreToolUse, None));
        let sessions = reg.list();
        assert_eq!(sessions.len(), 1, "same session_id upserts, not duplicates");
        assert_eq!(sessions[0].state, SessionState::Working);
        // The later `observe` had no cwd, but the known one is preserved.
        assert_eq!(sessions[0].cwd.as_deref(), Some(Path::new("/tmp/a")));
    }

    #[test]
    fn state_machine_covers_every_event() {
        use NotificationKind::*;
        use SessionEvent::*;
        let cases = [
            (SessionStart, SessionState::Starting),
            (UserPromptSubmit, SessionState::Working),
            (PreToolUse, SessionState::Working),
            (PostToolUse, SessionState::Working),
            (Stop, SessionState::Idle),
            (
                Notification(PermissionPrompt),
                SessionState::WaitingForPermission,
            ),
            (Notification(IdlePrompt), SessionState::WaitingForInput),
            (Notification(AgentNeedsInput), SessionState::WaitingForInput),
            (TranscriptGrew, SessionState::Working),
            (TranscriptDiscovered, SessionState::Idle),
        ];
        for (event, expected) in cases {
            assert_eq!(
                SessionState::for_event(&event, None),
                expected,
                "event {event:?}"
            );
        }
        // An unclassified notification keeps the current state.
        assert_eq!(
            SessionState::for_event(&Notification(Other), Some(SessionState::Working)),
            SessionState::Working
        );
        // TranscriptDiscovered on a known session keeps its state.
        assert_eq!(
            SessionState::for_event(&TranscriptDiscovered, Some(SessionState::Working)),
            SessionState::Working
        );
    }

    #[test]
    fn end_marks_ended_and_reaps_quickly() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/tmp/a"),
        ));
        assert!(reg.end("s1", Some("clear")));
        // Ending an unknown session is a no-op.
        assert!(!reg.end("ghost", None));
        let sessions = reg.list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, SessionState::Ended);
        // Age the ended entry past the short ended TTL: it reaps out.
        {
            let mut guard = reg.lock_sessions();
            guard.get_mut("s1").unwrap().last_seen = Utc::now() - chrono::Duration::seconds(30);
        }
        assert!(reg.list().is_empty(), "ended entry reaps after ended TTL");
    }

    #[test]
    fn stale_working_session_reaps_but_recent_survives() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("fresh", SessionEvent::PreToolUse, None));
        reg.observe(observe_request("stale", SessionEvent::PreToolUse, None));
        {
            let mut guard = reg.lock_sessions();
            guard.get_mut("stale").unwrap().last_seen =
                Utc::now() - chrono::Duration::seconds(1000);
        }
        let ids: Vec<String> = reg.list().into_iter().map(|s| s.session_id).collect();
        assert_eq!(ids, vec!["fresh".to_string()]);
    }

    #[test]
    fn source_is_vscode_when_cwd_is_under_a_reporting_window() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/home/me/proj/sub"),
        ));
        // A window reporting a Claude tab whose folder is a prefix of the cwd.
        reg.report_window(WindowReport {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/home/me/proj")],
            tabs: 1,
            terminals: 0,
        });
        let sessions = reg.list();
        assert_eq!(
            sessions[0].source,
            Source::VsCode {
                window_key: "w1".to_string()
            }
        );
    }

    #[test]
    fn source_is_terminal_when_window_has_no_embedding() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/home/me/proj"),
        ));
        // A window is open on the folder but has no Claude tab/terminal.
        reg.report_window(WindowReport {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/home/me/proj")],
            tabs: 0,
            terminals: 0,
        });
        assert_eq!(reg.list()[0].source, Source::Terminal);
    }

    #[test]
    fn window_report_is_upsert_and_unregister_removes() {
        let reg = SessionsRegistry::new();
        reg.report_window(WindowReport {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/p")],
            tabs: 1,
            terminals: 0,
        });
        // Upsert (same key) does not duplicate.
        reg.report_window(WindowReport {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/p")],
            tabs: 2,
            terminals: 1,
        });
        assert!(reg.unregister_window("w1"));
        assert!(!reg.unregister_window("w1"));
    }

    #[test]
    fn stale_window_stops_tagging_source() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/p/sub"),
        ));
        reg.report_window(WindowReport {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/p")],
            tabs: 1,
            terminals: 0,
        });
        // Age the window report past the window TTL.
        {
            let mut guard = reg.lock_windows();
            guard.get_mut("w1").unwrap().last_seen = Utc::now() - chrono::Duration::seconds(120);
        }
        assert_eq!(reg.list()[0].source, Source::Terminal);
    }

    #[test]
    fn resolve_source_prefers_lowest_key_on_overlap() {
        // Two windows both cover the cwd; the lowest key wins deterministically.
        let windows = vec![
            WindowReport {
                key: "w2".to_string(),
                folders: vec![PathBuf::from("/p")],
                tabs: 1,
                terminals: 0,
            },
            WindowReport {
                key: "w1".to_string(),
                folders: vec![PathBuf::from("/p")],
                tabs: 1,
                terminals: 0,
            },
        ];
        assert_eq!(
            resolve_source(Some(Path::new("/p/x")), &windows),
            Source::VsCode {
                window_key: "w1".to_string()
            }
        );
        // No cwd → terminal.
        assert_eq!(resolve_source(None, &windows), Source::Terminal);
    }

    #[test]
    fn focus_folder_resolves_matching_window_folder() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/home/me/proj/sub"),
        ));
        assert!(reg.focus_folder("s1").is_none(), "no window yet");
        reg.report_window(WindowReport {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/home/me/proj")],
            tabs: 1,
            terminals: 0,
        });
        assert_eq!(reg.focus_folder("s1"), Some(PathBuf::from("/home/me/proj")));
        // An unknown session resolves to nothing.
        assert!(reg.focus_folder("ghost").is_none());
    }

    #[test]
    fn evict_oldest_session_drops_the_longest_silent() {
        let now = Utc::now();
        let mut sessions = HashMap::new();
        for (id, age) in [("young", 0), ("old", 100), ("older", 200)] {
            sessions.insert(
                id.to_string(),
                SessionEntry {
                    session_id: id.to_string(),
                    cwd: None,
                    transcript_path: None,
                    repo: None,
                    model: None,
                    state: SessionState::Working,
                    source: Source::Terminal,
                    last_event: SessionEvent::PreToolUse,
                    started_at: now,
                    last_seen: now - chrono::Duration::seconds(age),
                },
            );
        }
        evict_oldest_session(&mut sessions);
        assert!(!sessions.contains_key("older"));
        assert!(sessions.contains_key("young"));
        assert!(sessions.contains_key("old"));
    }

    #[test]
    fn list_sorts_by_repo_then_session_id() {
        let reg = SessionsRegistry::new();
        for (id, repo) in [("z", "repo-a"), ("a", "repo-b"), ("m", "repo-a")] {
            reg.observe(ObserveRequest {
                session_id: id.to_string(),
                cwd: None,
                transcript_path: None,
                event: SessionEvent::PreToolUse,
                repo: Some(repo.to_string()),
                model: None,
            });
        }
        let ordered: Vec<(String, String)> = reg
            .list()
            .into_iter()
            .map(|s| (s.session_id, s.repo.unwrap()))
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("m".to_string(), "repo-a".to_string()),
                ("z".to_string(), "repo-a".to_string()),
                ("a".to_string(), "repo-b".to_string()),
            ]
        );
    }

    #[test]
    fn serialized_session_shapes_are_stable() {
        // The wire shape consumers (CLI, extension) read: snake_case state, a
        // tagged source, and omitted `None` fields.
        let reg = SessionsRegistry::new();
        reg.observe(ObserveRequest {
            session_id: "s1".to_string(),
            cwd: Some(PathBuf::from("/p")),
            transcript_path: None,
            event: SessionEvent::Notification(NotificationKind::PermissionPrompt),
            repo: Some("proj".to_string()),
            model: None,
        });
        let value = serde_json::to_value(&reg.list()[0]).unwrap();
        assert_eq!(value["state"], "waiting_for_permission");
        assert_eq!(value["source"]["kind"], "terminal");
        assert_eq!(value["repo"], "proj");
        // Absent optional fields are omitted, not null.
        assert!(value.get("model").is_none());
        assert!(value.get("transcript_path").is_none());
    }

    #[test]
    fn default_constructs_an_empty_registry() {
        let reg = SessionsRegistry::default();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn fill_only_overwrites_with_a_present_value() {
        // `None` leaves the slot; `Some` overwrites it — the re-`observe`
        // never-clobber contract.
        let mut slot = Some("keep");
        fill(&mut slot, None);
        assert_eq!(slot, Some("keep"));
        fill(&mut slot, Some("new"));
        assert_eq!(slot, Some("new"));
        // A previously-empty slot fills.
        let mut empty: Option<&str> = None;
        fill(&mut empty, Some("filled"));
        assert_eq!(empty, Some("filled"));
    }

    #[test]
    fn observe_at_session_cap_evicts_the_longest_silent() {
        let reg = SessionsRegistry::new();
        // Seed a full registry with explicit descending timestamps so the
        // highest-numbered id is unambiguously the oldest.
        {
            let mut sessions = reg.lock_sessions();
            let base = Utc::now();
            for i in 0..MAX_SESSIONS {
                let id = format!("s{i:04}");
                sessions.insert(
                    id.clone(),
                    SessionEntry {
                        session_id: id.clone(),
                        cwd: None,
                        transcript_path: None,
                        repo: None,
                        model: None,
                        state: SessionState::Working,
                        source: Source::Terminal,
                        last_event: SessionEvent::PreToolUse,
                        started_at: base,
                        last_seen: base - chrono::Duration::milliseconds(i as i64),
                    },
                );
            }
        }
        // A new session at the cap displaces exactly the longest-silent entry.
        reg.observe(observe_request("fresh", SessionEvent::PreToolUse, None));
        let sessions = reg.lock_sessions();
        assert_eq!(sessions.len(), MAX_SESSIONS);
        assert!(sessions.contains_key("fresh"));
        assert!(!sessions.contains_key(&format!("s{:04}", MAX_SESSIONS - 1)));
        assert!(sessions.contains_key("s0000"));
    }

    #[test]
    fn report_window_at_cap_evicts_the_longest_silent() {
        let reg = SessionsRegistry::new();
        {
            let mut windows = reg.lock_windows();
            let base = Utc::now();
            for i in 0..MAX_WINDOWS {
                let key = format!("w{i:04}");
                windows.insert(
                    key.clone(),
                    WindowEntry {
                        report: WindowReport {
                            key: key.clone(),
                            folders: vec![],
                            tabs: 1,
                            terminals: 0,
                        },
                        last_seen: base - chrono::Duration::milliseconds(i as i64),
                    },
                );
            }
        }
        reg.report_window(WindowReport {
            key: "fresh".to_string(),
            folders: vec![],
            tabs: 1,
            terminals: 0,
        });
        let windows = reg.lock_windows();
        assert_eq!(windows.len(), MAX_WINDOWS);
        assert!(windows.contains_key("fresh"));
        assert!(!windows.contains_key(&format!("w{:04}", MAX_WINDOWS - 1)));
        assert!(windows.contains_key("w0000"));
    }

    #[test]
    fn evict_oldest_window_breaks_ties_by_key() {
        let now = Utc::now();
        let mut windows = HashMap::new();
        let at = |key: &str, secs: i64| WindowEntry {
            report: WindowReport {
                key: key.to_string(),
                folders: vec![],
                tabs: 1,
                terminals: 0,
            },
            last_seen: now - chrono::Duration::seconds(secs),
        };
        windows.insert("young".to_string(), at("young", 0));
        windows.insert("old-b".to_string(), at("old-b", 10));
        windows.insert("old-a".to_string(), at("old-a", 10));
        // Oldest `last_seen` is shared; the lowest key loses.
        evict_oldest_window(&mut windows);
        assert!(!windows.contains_key("old-a"));
        assert!(windows.contains_key("old-b"));
        assert!(windows.contains_key("young"));
        // An empty map is a no-op, not a panic.
        let mut empty: HashMap<String, WindowEntry> = HashMap::new();
        evict_oldest_window(&mut empty);
        assert!(empty.is_empty());
    }
}
