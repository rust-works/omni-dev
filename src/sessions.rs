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
//!
//! The one exception is Feed 4, the [`stream`] tracker behind
//! `omni-dev claude-wrap`: it reads the exact state out of Claude's stream-json
//! stdio and reports it as [`SessionEvent::StreamState`], which
//! [`SessionState::for_event`] applies verbatim. See ADR-0057.
//!
//! Feed 5 is pi.dev's: a generated extension in `~/.pi/agent/extensions/` maps
//! pi's lifecycle events to a state and reports it the same way, tagged
//! [`Agent::Pi`] (#1901).
//!
//! Codex is a second hook feed: `omni-dev sessions hook --agent codex`, run from
//! `$CODEX_HOME/hooks.json`, maps Codex's events onto the same [`SessionEvent`]s
//! and tags its sessions [`Agent::Codex`] (#1907, ADR-0087). The
//! [`codex_watcher`] supplements it from Codex's rollout files and thread locks:
//! discovery, archive and killed-process ends, and idle liveness (#1909).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[cfg(unix)]
pub mod codex_app_server;
pub mod codex_watcher;
#[cfg(unix)]
pub mod journal;
pub(crate) mod pid_liveness;
pub(crate) mod pid_watcher;
pub mod relocate;
pub mod stream;
pub mod watcher;

/// How long a session may go silent before it ages out of the registry, absent
/// a confirmed-alive pid.
///
/// Unlike a VS Code window (which heartbeats every ~10s), a running Claude
/// session emits nothing while idle at the prompt, so its only liveness signal
/// used to be activity — a hook event or transcript growth. The TTL is
/// generous for exactly that reason: a session that has done nothing for this
/// long is assumed gone (a `claude` that exited without firing `SessionEnd`)
/// and reaped on the next read.
///
/// The TTL is measured in *awake* time (#2108): see [`AwakeClock`]. A system
/// sleep of any length ages no session, so the first read after a wake-up no
/// longer reaps every live one.
///
/// Since #1916, a session whose pid the [`pid_watcher`] has
/// independently confirmed alive is kept out of this TTL's reach entirely: the
/// watcher refreshes `last_active` itself on a schedule of its own, off this
/// lock, so `reap_sessions` never needs to know about pids at all. This TTL is
/// now the fallback for a session with no pid (older hooks, the transcript
/// watcher, pi.dev), one the watcher has not yet confirmed, or whose pid it
/// can't confirm (an unprompted spare process, one another `session_id`
/// has since taken over, or one the stream wrapper reports — #1454). A
/// still-alive idle session bound by this TTL re-appears the moment it next
/// does anything. See ADR-0052.
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
/// embedded on the next read. Measured in awake time like the session TTL
/// (#2108): a window cannot heartbeat while the machine sleeps either.
const DEFAULT_WINDOW_TTL: Duration = Duration::from_secs(30);

/// Ceiling on live session entries, so a runaway feed cannot grow daemon memory
/// faster than the TTL reaps it (the worktrees `MAX_WINDOWS` precedent, #1140).
/// Far above any real concurrent-session count; at the cap a genuinely new
/// session evicts the longest-silent entry rather than being rejected, so ingest
/// stays infallible.
const MAX_SESSIONS: usize = 512;

/// How many replaced agent processes a session remembers (#1948). Enough for a
/// burst of window reloads, each of which replaces the previous process before
/// its `SessionEnd` has necessarily arrived; the oldest is forgotten first.
const MAX_REPLACED_PIDS: usize = 8;

/// Ceiling on live window-embedding reports, mirroring the worktrees registry cap.
const MAX_WINDOWS: usize = 256;

/// How many recent event `seq`s a session remembers, to drop the copy of an
/// event that reaches the registry by a second route (#2108). A hook delivers
/// each event twice — once on the socket, once through its journal — and only
/// the first may count.
const MAX_RECENT_SEQS: usize = 32;

/// The identity of one hook event: when it fired and a per-event nonce.
///
/// Stamped by the hook sink, written into the event's journal record and sent
/// again on the socket POST, so the daemon can tell a copy it already applied
/// from one it has not (#2108). `seq` is unique per event across processes
/// (`<hook pid>-<unix nanos>`); `ts` is the sink's wall clock at the moment the
/// event arrived, which orders a journal replay against newer events that
/// already got through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventStamp {
    /// When the hook fired, by the sink's wall clock.
    pub ts: DateTime<Utc>,
    /// The event's nonce.
    pub seq: String,
}

/// Which route an event reached the registry by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The control socket: the fast path, and what every feed but the journal
    /// uses. Always applied, as before — only an exact duplicate is dropped.
    Socket,
    /// A hook's durable journal, read late: replayed at startup, or caught up
    /// after a dropped POST. Also dropped when older than what the session has
    /// already applied, so a late-read event can never undo a newer one.
    Journal,
}

/// A monotonic clock that stands still while the machine sleeps, so a TTL
/// measured on it counts only the time the daemon could actually have heard
/// from a session (#2108).
///
/// The wall clock is the wrong ruler for a liveness TTL: while the system sleeps
/// nothing refreshes a session — no hooks, no Codex lock heartbeat, no pid
/// watcher — so the first read after a wake-up used to find every entry "stale"
/// and reap the lot. [`Instant`] does not advance across suspend on the
/// platforms the daemon runs on (`CLOCK_UPTIME_RAW` on macOS, `CLOCK_MONOTONIC`
/// on Linux), so entries stamped with it survive a sleep of any length and are
/// reaped only after a TTL's worth of *awake* silence.
///
/// Stamps are a [`Duration`] since the registry was created rather than a raw
/// [`Instant`], so tests can move the clock forward without subtracting from an
/// `Instant` (which panics on a host with little uptime).
#[derive(Debug)]
struct AwakeClock {
    /// The registry's creation, the zero of every stamp.
    base: Instant,
    /// Test-only extra elapsed time, in milliseconds.
    #[cfg(test)]
    skew_ms: std::sync::atomic::AtomicU64,
}

impl AwakeClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            #[cfg(test)]
            skew_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Awake time elapsed since the registry was created.
    fn now(&self) -> Duration {
        let elapsed = self.base.elapsed();
        #[cfg(test)]
        let elapsed = elapsed
            + Duration::from_millis(self.skew_ms.load(std::sync::atomic::Ordering::Relaxed));
        elapsed
    }

    /// Moves the clock forward, as if the machine had been awake that long.
    #[cfg(test)]
    fn advance(&self, by: Duration) {
        self.skew_ms.fetch_add(
            u64::try_from(by.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

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
    /// - `UserPromptSubmit` / `PreToolUse` / `PostToolUse` →
    ///   [`Working`](Self::Working)
    /// - `TranscriptGrew` → [`Working`](Self::Working), **except** while
    ///   [`Starting`](Self::Starting) /
    ///   [`WaitingForInput`](Self::WaitingForInput) /
    ///   [`WaitingForPermission`](Self::WaitingForPermission) /
    ///   [`Ended`](Self::Ended), which it leaves **unchanged** — growth is
    ///   expected in those states without the session doing anything, so it is
    ///   not evidence a turn is running (#1418, #1946)
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
    /// - `StreamState(s)` → `s` verbatim (an authoritative stream-json report;
    ///   the only non-inferred variant — see ADR-0057)
    #[must_use]
    pub fn for_event(event: &SessionEvent, current: Option<Self>) -> Self {
        match event {
            SessionEvent::SessionStart => Self::Starting,
            SessionEvent::UserPromptSubmit
            | SessionEvent::PreToolUse
            | SessionEvent::PostToolUse => Self::Working,
            // Growth is only evidence the transcript file got bigger. From most
            // states that does imply a turn is running, but in two it does not,
            // and reading it as `working` would overwrite a state a hook
            // reported directly:
            //
            // - `waiting_for_*` — Claude flushes the assistant `tool_use` line
            //   *before* the prompt it is asking about can be answered, so the
            //   watcher's next scan would downgrade the wait and the row would
            //   go quiet exactly when it should be shouting (#1418);
            // - `ended` — a session's last lines land around `SessionEnd`, so a
            //   scan inside the ended-linger window would revive the entry and
            //   hold a phantom `working` row for the whole session TTL;
            // - `starting` — a resumed session (a VS Code window reload,
            //   `claude --resume`) keeps its id, and the *old* process appends a
            //   `cost-state` line to the shared transcript as it exits. The 5s
            //   scan usually sees that write after the new `SessionStart`, and
            //   no hook fires while an unprompted session sits idle, so reading
            //   it as `working` would pin the row busy until the next prompt
            //   (#1946). Nothing is lost: a hook-fed session's first prompt
            //   fires `UserPromptSubmit`, and a watcher-only session never
            //   reaches `starting`, because `SessionStart` is a hook.
            //
            // That is ADR-0052's reliable-over-inferred ordering, and the rule
            // `stream.rs`'s `state` already applies to a permission prompt. Each
            // is released by any later hook, which is inference-free.
            SessionEvent::TranscriptGrew => match current {
                Some(
                    held @ (Self::Starting
                    | Self::WaitingForInput
                    | Self::WaitingForPermission
                    | Self::Ended),
                ) => held,
                _ => Self::Working,
            },
            SessionEvent::Stop => Self::Idle,
            // An authoritative state from a stream-json observer wins outright,
            // ignoring the inferred `current` — it read the exact state from the
            // stream rather than guessing from a lifecycle event (ADR-0057).
            SessionEvent::StreamState(state) => *state,
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
    /// An **authoritative** state reported directly by a stream-json observer —
    /// the `omni-dev claude-wrap` wrapper reading Claude's `--output-format
    /// stream-json` stdout, where the exact state is first-class (`init` →
    /// working, `result` → idle, `can_use_tool` → waiting-for-permission). Unlike
    /// every other variant this is *not* inferred: [`SessionState::for_event`]
    /// returns the carried state verbatim. Serialized as
    /// `{"stream_state":"waiting_for_permission"}` (ADR-0057).
    StreamState(SessionState),
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

/// Which coding agent a session belongs to.
///
/// The registry is keyed by `session_id` alone. Claude Code's ids are UUID v4,
/// while pi.dev's and Codex's are both UUID v7, so the version nibble no longer
/// separates every pair (#1907): single-key identity instead rests on v7's 74
/// random bits, which make a pi/Codex collision improbable — not impossible, and
/// not worth a re-key (ADR-0087). The tag lets a consumer tell the agents apart.
/// Serialized `snake_case`; absent on the wire means [`Claude`](Self::Claude), so
/// the Claude feeds (hooks, watcher, `claude-wrap`) send it by omission and stay
/// byte-identical to senders that predate it (#1901).
///
/// A daemon built with an older variant set rejects an unknown value
/// (`unknown variant`), so a newer sink's `observe` is dropped until the daemon
/// is upgraded — the sinks are fail-open, so that is silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    /// Claude Code — every feed but the pi extension.
    #[default]
    Claude,
    /// pi.dev's coding agent, reported by the extension `sessions install-hooks`
    /// writes into `~/.pi/agent/extensions/`.
    Pi,
    /// OpenAI Codex (CLI, VS Code extension and Desktop), reported by
    /// `sessions hook --agent codex` from `$CODEX_HOME/hooks.json` (#1907).
    Codex,
}

impl Agent {
    /// The agent's display name, for the tray and other human-facing labels.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Pi => "pi",
            Self::Codex => "Codex",
        }
    }

    /// Whether this is the default agent, so [`ObserveRequest`] can omit it.
    #[must_use]
    pub fn is_claude(&self) -> bool {
        *self == Self::Claude
    }
}

/// An idempotent session sighting sent to the registry — the wire payload of the
/// `observe` op, and the argument to [`SessionsRegistry::observe`].
///
/// The hook sink and the transcript watcher both produce these; every field but
/// `session_id` and `event` is best-effort and *fills in* missing data on an
/// existing entry without ever clobbering known data with `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveRequest {
    /// Claude subagent identity; absent for parent hooks and other feeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
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
    /// Which agent the session belongs to; omitted for Claude Code.
    #[serde(default, skip_serializing_if = "Agent::is_claude")]
    pub agent: Agent,
    /// The agent process's pid, when known: the hook sink's parent, or
    /// `claude-wrap`'s wrapped child. Tells a resumed session's new process
    /// from the old one it replaced, which share a `session_id` (#1948). Every
    /// other feed (the watchers, the Codex app-server, pi.dev) sends `None`.
    ///
    /// This is also the seed for pid-based liveness (#1916): the
    /// [`pid_watcher`] independently probes every pid it sees here, off this
    /// wire and off the registry lock, rather than trusting a client-supplied
    /// process identity — see [`SessionEntry::pid_start`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
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
    /// Parent state hidden by outstanding subagent waits.
    #[serde(skip)]
    pub(crate) subagent_base: Option<SessionState>,
    /// Outstanding waits, released only by their owning subagent.
    #[serde(skip)]
    pub(crate) subagent_waits: HashMap<String, SessionState>,
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
    /// Which agent the session belongs to. Set by the first sighting and never
    /// changed, since a session id belongs to exactly one agent.
    pub agent: Agent,
    /// The current inferred state.
    pub state: SessionState,
    /// Where the session runs, resolved on read.
    pub source: Source,
    /// The most recent event observed for this session.
    pub last_event: SessionEvent,
    /// When the session was first observed (RFC 3339).
    pub started_at: DateTime<Utc>,
    /// When the registry last heard from this session (RFC 3339). Wall-clock,
    /// for display only: liveness is measured on `last_active`.
    pub last_seen: DateTime<Utc>,
    /// When the registry last heard from this session, on the registry's
    /// [`AwakeClock`] — the stamp [`reap_sessions`] compares against, so a system
    /// sleep never ages an entry (#2108).
    #[serde(skip)]
    pub(crate) last_active: Duration,
    /// The agent process that owns the session: the pid of its latest
    /// pid-bearing sighting that did not come from a replaced process.
    #[serde(skip)]
    pub(crate) pid: Option<u32>,
    /// `pid`'s start-time identity token (#1916), **set only by the
    /// [`pid_watcher`]**, never from a client-supplied value: it is captured
    /// the first time the watcher independently confirms `pid` alive, and
    /// reset to `None` whenever `pid` changes owner (a new pid's identity is
    /// unconfirmed until the watcher next probes it). `None` until then, so a
    /// pid the watcher has not yet gotten to on its own schedule simply has no
    /// opinion on liveness rather than a stale or spoofable one.
    #[serde(skip)]
    pub(crate) pid_start: Option<String>,
    /// Whether this session has had at least one `UserPromptSubmit`. Gates the
    /// pid-liveness TTL exemption (#1916): a spare process VS Code keeps alive
    /// that has never been prompted must not be pinned forever just because its
    /// pid is alive.
    #[serde(skip)]
    pub(crate) prompted: bool,
    /// Whether a Claude `claude-wrap` stream observer has reported this session
    /// (an authoritative [`SessionEvent::StreamState`]). Excludes it from the
    /// pid-liveness TTL exemption (#1454): the stream wrapper is only ever
    /// attached to a VS Code/SDK-driven `claude`, whose extension keeps a
    /// process per chat rather than per visible tab, so a live pid says nothing
    /// about whether anyone can still see the chat. Its busy states are kept
    /// fresh by the wrapper's own keep-alive instead. Sticky across hooks, but
    /// reset when a different process takes the session over ([`track_pid`]).
    #[serde(skip)]
    pub(crate) streamed: bool,
    /// The processes the session was taken over from, oldest first and capped
    /// at [`MAX_REPLACED_PIDS`] — the old processes of in-place resumes. An
    /// `end` from one of them is ignored (#1948).
    #[serde(skip)]
    pub(crate) replaced_pids: VecDeque<u32>,
    /// The `seq`s of the most recent stamped events applied, newest last and
    /// capped at [`MAX_RECENT_SEQS`], so the second copy of an event (socket and
    /// journal both deliver every hook) is dropped (#2108).
    #[serde(skip)]
    pub(crate) recent_seqs: VecDeque<String>,
    /// The newest `ts` among the stamped events applied: a journal event older
    /// than this is stale and dropped (#2108).
    #[serde(skip)]
    pub(crate) latest_stamp_ts: Option<DateTime<Utc>>,
}

impl SessionEntry {
    /// Why a stamped event must not be applied to this session, if so: it is a
    /// duplicate of one already applied, or (journal route only) older than one.
    fn stamp_skip(&self, stamp: Option<&EventStamp>, origin: Origin) -> Option<&'static str> {
        let stamp = stamp?;
        if self.recent_seqs.contains(&stamp.seq) {
            return Some("duplicate_ignored");
        }
        if origin == Origin::Journal && self.latest_stamp_ts.is_some_and(|latest| stamp.ts < latest)
        {
            return Some("journal_stale_ignored");
        }
        None
    }

    /// Remembers an applied event's stamp for [`stamp_skip`](Self::stamp_skip).
    fn record_stamp(&mut self, stamp: &EventStamp) {
        if self.recent_seqs.len() >= MAX_RECENT_SEQS {
            self.recent_seqs.pop_front();
        }
        self.recent_seqs.push_back(stamp.seq.clone());
        // The newest, not the latest to arrive: events from concurrent hooks can
        // reach the socket out of order, and a journal event older than any of
        // them is stale either way.
        self.latest_stamp_ts = Some(self.latest_stamp_ts.map_or(stamp.ts, |t| t.max(stamp.ts)));
    }
}

/// When an event is taken to have been seen, for the wall-clock `last_seen`: now
/// for the socket, and the event's own `ts` for a journal event (never in the
/// future), so a replayed history cannot look fresh (#2108).
fn seen_at(stamp: Option<&EventStamp>, origin: Origin, now: DateTime<Utc>) -> DateTime<Utc> {
    match (origin, stamp) {
        (Origin::Journal, Some(stamp)) => stamp.ts.min(now),
        _ => now,
    }
}

/// Subagent activity shares the parent's session id but does not run its turn.
fn subagent_state(
    entry: &mut SessionEntry,
    event: SessionEvent,
    agent_id: Option<&str>,
) -> SessionState {
    if let Some(id) = agent_id.filter(|id| !id.trim().is_empty()) {
        match event {
            SessionEvent::Notification(
                NotificationKind::PermissionPrompt
                | NotificationKind::IdlePrompt
                | NotificationKind::AgentNeedsInput,
            ) => {
                entry.subagent_base.get_or_insert(entry.state);
                entry
                    .subagent_waits
                    .insert(id.to_owned(), SessionState::for_event(&event, None));
            }
            SessionEvent::PostToolUse => {
                entry.subagent_waits.remove(id);
            }
            _ => {}
        }
        if entry
            .subagent_waits
            .values()
            .any(|s| *s == SessionState::WaitingForPermission)
        {
            return SessionState::WaitingForPermission;
        }
        if !entry.subagent_waits.is_empty() {
            return SessionState::WaitingForInput;
        }
        return entry.subagent_base.take().unwrap_or(entry.state);
    }
    let next = SessionState::for_event(&event, Some(entry.state));
    // Passive evidence cannot release a wait. Parent hooks remain the fallback
    // release when the parent Task completes or a new turn starts.
    if !matches!(
        event,
        SessionEvent::TranscriptDiscovered
            | SessionEvent::TranscriptGrew
            | SessionEvent::Notification(NotificationKind::Other)
    ) {
        entry.subagent_waits.clear();
        entry.subagent_base = None;
    }
    next
}

/// One pid-bearing session, as [`SessionsRegistry::pid_liveness_candidates`]
/// hands it to the [`pid_watcher`](pid_watcher) — a plain data snapshot with no
/// lock and no process handle attached, so the watcher can probe it entirely
/// off the registry lock.
#[derive(Debug, Clone)]
pub(crate) struct PidCandidate {
    pub(crate) session_id: String,
    pub(crate) pid: u32,
    /// The identity token last confirmed for `pid`, if any (see
    /// [`SessionEntry::pid_start`]).
    pub(crate) pid_start: Option<String>,
    pub(crate) prompted: bool,
    /// Whether the session is reported by the stream wrapper — see
    /// [`SessionEntry::streamed`]. Such a session is never exempted from the TTL.
    pub(crate) streamed: bool,
    /// When this session was last seen, used to pick the *currently active*
    /// session_id among several sharing a pid — not `started_at`, which would
    /// wrongly keep favouring a `/clear`-abandoned session forever over one a
    /// later `/resume` reactivated, since creation order never changes but
    /// `last_seen` moves with whichever session_id is actually receiving hooks.
    pub(crate) last_seen: DateTime<Utc>,
}

/// One companion window-embedding report, with its liveness stamp.
#[derive(Debug, Clone)]
struct WindowEntry {
    /// The report as sent by the companion.
    report: WindowReport,
    /// When the report last arrived (register or refresh), on the registry's
    /// [`AwakeClock`]. [`reap_windows`] compares against it, so a system sleep
    /// never ages a window report (#2108); a window's wall-clock arrival time is
    /// shown nowhere, so none is kept.
    last_active: Duration,
    /// When this key first registered, preserved across refreshes. Ranks
    /// overlapping windows in [`pick_window`] (#1451): a reload registers a new
    /// key, so it outranks the stale predecessor still inside its TTL, while
    /// two live windows keep a stable winner — `last_seen` would flap between
    /// them on every heartbeat.
    registered_at: DateTime<Utc>,
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
    /// The clock every TTL is measured on; see [`AwakeClock`].
    clock: AwakeClock,
    /// A monotonically-bumped version counter, incremented whenever the state a
    /// subscriber renders changes. A push-subscription consumer holds a
    /// [`watch::Receiver`] from [`subscribe_changes`](Self::subscribe_changes)
    /// and wakes on each bump to re-snapshot (#1414) — the
    /// [`WorktreesRegistry`](crate::worktrees::WorktreesRegistry) arrangement,
    /// one service over. The counter's *value* is immaterial — only that it
    /// changed — so a burst coalesces into one wake and the server diffs the
    /// resulting snapshot to suppress duplicate frames.
    ///
    /// `watch` needs no runtime and never blocks, so it fits this engine's
    /// no-async-setup posture; every [`bump`](Self::bump) happens *after* the map
    /// guard is dropped, so the `std::Mutex`-never-across-`.await` rule is intact
    /// (and the watch's own internal lock is never nested under a map lock).
    changes: watch::Sender<u64>,
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
            clock: AwakeClock::new(),
            changes: watch::channel(0).0,
        }
    }

    /// A change-notification receiver for the push subscription: it observes a
    /// new value each time the rendered session state changes (see
    /// [`bump`](Self::bump)). Created with the current version already marked
    /// seen, so the first [`watch::Receiver::changed`] resolves on the *next*
    /// change — the subscriber sends its own initial snapshot up front and then
    /// waits for deltas (#1414).
    #[must_use]
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Signals subscribers that the rendered state changed. Non-blocking and
    /// runtime-free; called only *after* a map guard is released so the locks
    /// never nest. A send never fails here (the sender is owned by the registry,
    /// which outlives every receiver, and `send_modify` bumps even with no
    /// receivers).
    ///
    /// Callers bump **only on a change a consumer renders** — a new or dropped
    /// session, a [`SessionState`] transition, a best-effort field taking a new
    /// value, or a window report that alters the [`Source`] join. Deliberately
    /// *narrower* than "the serialized payload differs": [`SessionEntry`] carries
    /// `last_seen` and `last_event`, which churn on every hook event, so bumping
    /// on those would push a fresh snapshot to every window on every `PreToolUse`
    /// with the server's snapshot diff unable to suppress any of it. Their deltas
    /// ride the server's periodic re-sample instead — the same latency the poll
    /// this replaced already had, for fields nothing renders.
    pub(crate) fn bump(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
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
    ///
    /// [`bump`](Self::bump)s only when the sighting changed something a consumer
    /// renders — a brand-new session, a [`SessionState`] transition, a
    /// best-effort field taking a new value, or a reap that dropped a sibling.
    /// A repeat sighting that merely refreshes liveness does not, since hooks
    /// fire on every tool call (the `heartbeat` precedent in
    /// [`WorktreesRegistry`](crate::worktrees::WorktreesRegistry)).
    pub fn observe(&self, req: ObserveRequest) {
        self.observe_stamped(req, None, Origin::Socket);
    }

    /// [`observe`](Self::observe) for an event that carries an [`EventStamp`] and
    /// arrived by `origin` (#2108). A duplicate of an event already applied is
    /// dropped whatever its route, and a [`Journal`](Origin::Journal) event older
    /// than one already applied is dropped too; everything else behaves exactly as
    /// `observe`, except that a journal event's wall-clock `last_seen` is the
    /// event's own timestamp rather than now.
    pub(crate) fn observe_stamped(
        &self,
        req: ObserveRequest,
        stamp: Option<EventStamp>,
        origin: Origin,
    ) {
        let agent_id = req
            .agent_id
            .as_deref()
            .filter(|id| req.agent == Agent::Claude && !id.trim().is_empty());
        let session_id = req.session_id.clone();
        let event = req.event;
        let agent = req.agent;
        let pid = req.pid;
        let now = Utc::now();
        let seen = seen_at(stamp.as_ref(), origin, now);
        let awake = self.clock.now();
        let (changed, old_state, new_state, outcome, reaped) = {
            let mut sessions = self.lock_sessions();
            let reaped = reap_sessions(&mut sessions, self.session_ttl, self.ended_ttl, awake);
            let old_state = sessions.get(&session_id).map(|entry| entry.state);
            let skip = sessions
                .get(&session_id)
                .and_then(|entry| entry.stamp_skip(stamp.as_ref(), origin));
            let mut outcome = "created";
            let mutated = match sessions.get_mut(&req.session_id) {
                // The second copy of an event already applied, or a journal
                // event older than one that was (#2108).
                Some(_) if skip.is_some() => {
                    outcome = skip.unwrap_or("skipped");
                    false
                }
                // A passive re-sighting (the Codex rollout watcher's heartbeat)
                // must not refresh an ended session, or it would outlive its
                // short ended-linger window (#1909).
                Some(entry)
                    if entry.state == SessionState::Ended
                        && (req.event == SessionEvent::TranscriptDiscovered
                            || agent_id.is_some()) =>
                {
                    outcome = "ended_passive_ignored";
                    false
                }
                // A straggling sighting from a process this session's resume
                // already replaced: ignore it entirely, so a delayed hook from
                // the old process (anything short of its `SessionEnd`, handled
                // separately by `end`) cannot move the resumed session's state
                // out from under the new process that owns it (#1948).
                Some(entry)
                    if req
                        .pid
                        .is_some_and(|pid| entry.replaced_pids.contains(&pid)) =>
                {
                    outcome = "replaced_pid_ignored";
                    false
                }
                Some(entry) => {
                    track_pid(entry, req.pid);
                    entry.prompted |= req.event == SessionEvent::UserPromptSubmit;
                    entry.streamed |= is_claude_stream_report(&req);
                    let next = subagent_state(entry, req.event, agent_id);
                    let state_changed = next != entry.state;
                    entry.state = next;
                    entry.last_event = req.event;
                    entry.last_seen = if origin == Origin::Journal {
                        entry.last_seen.max(seen)
                    } else {
                        now
                    };
                    entry.last_active = awake;
                    // Bound to locals rather than folded into the `||` below: every
                    // field must be filled, and short-circuiting would skip the rest.
                    let filled_cwd = fill(&mut entry.cwd, req.cwd);
                    let filled_transcript = fill(&mut entry.transcript_path, req.transcript_path);
                    let filled_repo = fill(&mut entry.repo, req.repo);
                    let filled_model = fill(&mut entry.model, req.model);
                    let metadata_changed =
                        filled_cwd || filled_transcript || filled_repo || filled_model;
                    outcome = if state_changed {
                        "state_changed"
                    } else if metadata_changed {
                        "metadata_enriched"
                    } else {
                        "heartbeat_only"
                    };
                    state_changed || metadata_changed
                }
                None => {
                    if sessions.len() >= MAX_SESSIONS {
                        evict_oldest_session(&mut sessions);
                    }
                    let state = if agent_id.is_some() {
                        SessionState::Idle
                    } else {
                        SessionState::for_event(&req.event, None)
                    };
                    let prompted = req.event == SessionEvent::UserPromptSubmit;
                    let streamed = is_claude_stream_report(&req);
                    let session_id = req.session_id.clone();
                    let mut entry = SessionEntry {
                        subagent_base: None,
                        subagent_waits: HashMap::new(),
                        session_id: req.session_id,
                        cwd: req.cwd,
                        transcript_path: req.transcript_path,
                        repo: req.repo,
                        model: req.model,
                        agent: req.agent,
                        state,
                        source: Source::Terminal,
                        last_event: req.event,
                        started_at: seen,
                        last_seen: seen,
                        last_active: awake,
                        pid: req.pid,
                        pid_start: None,
                        prompted,
                        streamed,
                        replaced_pids: VecDeque::new(),
                        recent_seqs: VecDeque::new(),
                        latest_stamp_ts: None,
                    };
                    entry.state = subagent_state(&mut entry, req.event, agent_id);
                    sessions.insert(session_id, entry);
                    true
                }
            };
            if skip.is_none() {
                if let (Some(stamp), Some(entry)) = (&stamp, sessions.get_mut(&session_id)) {
                    entry.record_stamp(stamp);
                }
            }
            (
                mutated || reaped > 0,
                old_state,
                sessions.get(&session_id).map(|entry| entry.state),
                outcome,
                reaped,
            )
        };
        if changed {
            self.bump();
        }
        tracing::debug!(%session_id, ?agent, ?pid, ?agent_id, ?event, ?old_state, ?new_state, outcome,
            ?origin, reaped, bumped = changed, "session_observed");
    }

    /// Marks a session ended (`SessionEnd`), so `list` shows it as `ended` for a
    /// short window ([`ENDED_SESSION_TTL`]) before it is reaped. Returns whether
    /// the session was known. A no-op for an already-unknown session (a
    /// duplicate/late `SessionEnd`).
    ///
    /// Also a no-op when `pid` is a process the session was taken over from:
    /// an in-place resume (a VS Code window reload) starts a new process on the
    /// same `session_id` without waiting for the old one to exit, so the old
    /// process's `SessionEnd` can arrive after the new one's `SessionStart`
    /// (#1948). Any other `end` — no pid, the owning pid, or a pid never seen
    /// (a wrapped hook command whose parent is a per-hook shell) — ends the
    /// session, so the rule can only ever keep a session the old process no
    /// longer owns.
    pub fn end(&self, session_id: &str, reason: Option<&str>, pid: Option<u32>) -> bool {
        self.end_stamped(session_id, reason, pid, None, Origin::Socket)
    }

    /// [`end`](Self::end) for an event that carries an [`EventStamp`] and arrived
    /// by `origin` (#2108): a duplicate, or a journal `SessionEnd` older than an
    /// event the session has since applied (a resume), is ignored.
    pub(crate) fn end_stamped(
        &self,
        session_id: &str,
        _reason: Option<&str>,
        pid: Option<u32>,
        stamp: Option<EventStamp>,
        origin: Origin,
    ) -> bool {
        let now = Utc::now();
        let seen = seen_at(stamp.as_ref(), origin, now);
        let awake = self.clock.now();
        let (known, reaped, old_state, outcome) = {
            let mut sessions = self.lock_sessions();
            let reaped = reap_sessions(&mut sessions, self.session_ttl, self.ended_ttl, awake);
            let old_state = sessions.get(session_id).map(|entry| entry.state);
            let skip = sessions
                .get(session_id)
                .and_then(|entry| entry.stamp_skip(stamp.as_ref(), origin));
            let mut outcome = "unknown";
            let known = match sessions.get_mut(session_id) {
                Some(_) if skip.is_some() => {
                    outcome = skip.unwrap_or("skipped");
                    (true, false)
                }
                // Already ended (a hook and a watcher can both end it): leave the
                // linger window alone rather than restart it.
                Some(entry) if entry.state == SessionState::Ended => {
                    outcome = "already_ended";
                    (true, false)
                }
                // The replaced process's late `SessionEnd`: the resumed session
                // lives on under its new process.
                Some(entry) if pid.is_some_and(|pid| entry.replaced_pids.contains(&pid)) => {
                    outcome = "replaced_pid_ignored";
                    (true, false)
                }
                Some(entry) => {
                    outcome = "ended";
                    entry.state = SessionState::Ended;
                    entry.last_event = SessionEvent::Stop;
                    entry.last_seen = if origin == Origin::Journal {
                        entry.last_seen.max(seen)
                    } else {
                        now
                    };
                    entry.last_active = awake;
                    (true, true)
                }
                None => (false, false),
            };
            if skip.is_none() {
                if let (Some(stamp), Some(entry)) = (&stamp, sessions.get_mut(session_id)) {
                    entry.record_stamp(stamp);
                }
            }
            (known, reaped, old_state, outcome)
        };
        let (known, flipped) = known;
        // A known session flipped to `ended`; otherwise only this call's inline
        // reap could have changed anything.
        let bumped = flipped || reaped > 0;
        if bumped {
            self.bump();
        }
        tracing::debug!(
            session_id,
            ?pid,
            ?old_state,
            outcome,
            ?origin,
            reaped,
            bumped,
            "session_end"
        );
        known
    }

    /// A cheap, lock-scoped snapshot for the [`pid_watcher`](pid_watcher): one
    /// [`PidCandidate`] per live, non-`ended` session that carries a pid — the
    /// watcher does every I/O-bearing liveness check off this lock, on its own
    /// schedule, so this method itself touches no process and does no I/O.
    pub(crate) fn pid_liveness_candidates(&self) -> Vec<PidCandidate> {
        self.lock_sessions()
            .values()
            .filter(|e| e.state != SessionState::Ended)
            .filter_map(|e| {
                Some(PidCandidate {
                    session_id: e.session_id.clone(),
                    pid: e.pid?,
                    pid_start: e.pid_start.clone(),
                    prompted: e.prompted,
                    streamed: e.streamed,
                    last_seen: e.last_seen,
                })
            })
            .collect()
    }

    /// Records that the [`pid_watcher`](pid_watcher) has independently
    /// confirmed `session_id`'s pid alive: refreshes `last_seen` so
    /// [`reap_sessions`] never sees it go stale, and captures `pid_start` when
    /// the entry does not already have one (the watcher's first confirmation of
    /// a given owner pid). A no-op for an unknown or already-`ended` session —
    /// the watcher's snapshot can be one tick stale by the time it applies a
    /// decision. Never bumps: this only refreshes fields nothing renders,
    /// exactly like a window's unchanged heartbeat refresh.
    pub(crate) fn confirm_pid_liveness(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
        pid_start: &str,
    ) {
        let awake = self.clock.now();
        let mut sessions = self.lock_sessions();
        if let Some(entry) = sessions.get_mut(session_id) {
            if entry.state != SessionState::Ended {
                entry.last_seen = now;
                entry.last_active = awake;
                if entry.pid_start.is_none() {
                    entry.pid_start = Some(pid_start.to_string());
                }
            }
        }
    }

    /// Records (upserts) a companion window-embedding report and refreshes its
    /// liveness. Reaps stale windows first, then caps like [`observe`](Self::observe).
    ///
    /// [`bump`](Self::bump)s only when the report changes the [`Source`] join a
    /// consumer renders — a new window, different `folders`, or an embedding that
    /// appeared or vanished — never on the unchanged ~10 s refresh every open
    /// window sends, which would otherwise put a permanent push floor under the
    /// daemon proportional to the window count.
    pub fn report_window(&self, report: WindowReport) {
        let window_key = report.key.clone();
        let folder_count = report.folders.len();
        let now = Utc::now();
        let awake = self.clock.now();
        let (changed, outcome, reaped) = {
            let mut windows = self.lock_windows();
            let reaped = reap_windows(&mut windows, self.window_ttl, awake);
            let outcome = if windows.contains_key(&report.key) {
                "refresh"
            } else {
                "registered"
            };
            let (mutated, registered_at) = if let Some(previous) = windows.get(&report.key) {
                (
                    previous.report.folders != report.folders
                        || previous.report.has_embedding() != report.has_embedding(),
                    previous.registered_at,
                )
            } else {
                if windows.len() >= MAX_WINDOWS {
                    evict_oldest_window(&mut windows);
                }
                (true, now)
            };
            windows.insert(
                report.key.clone(),
                WindowEntry {
                    report,
                    last_active: awake,
                    registered_at,
                },
            );
            (
                mutated || reaped > 0,
                if outcome == "refresh" && mutated {
                    "embedding_changed"
                } else {
                    outcome
                },
                reaped,
            )
        };
        if changed {
            self.bump();
        }
        tracing::debug!(%window_key, folder_count, outcome, reaped, bumped = changed, "session_window_reported");
    }

    /// Drops a companion window-embedding report (the window closed). Returns
    /// whether an entry was present.
    pub fn unregister_window(&self, key: &str) -> bool {
        let removed = {
            let mut windows = self.lock_windows();
            windows.remove(key).is_some()
        };
        if removed {
            self.bump();
        }
        tracing::debug!(
            window_key = key,
            removed,
            bumped = removed,
            "session_window_unregistered"
        );
        removed
    }

    /// Pauses the window-report liveness clock for `outage`: advances every live
    /// report's `last_active` by that long, never past now (#2111).
    ///
    /// For a stretch when the daemon could not accept connections, so no window's
    /// ~10 s refresh could arrive. Without this the first `window` op after the
    /// outage reaps every other report that has been quiet more than the 30 s TTL,
    /// and the sessions in those windows flip from `vscode` to `terminal` until each
    /// window reports again — the same failure
    /// [`WorktreesRegistry::credit_outage`] fixes for the window registry.
    ///
    /// `outage` is measured on a monotonic [`Instant`](std::time::Instant), the
    /// same awake time as the [`AwakeClock`] the reports are stamped on, so the
    /// two compose: the credit shifts a stamp by time the daemon was awake but
    /// deaf, and a system sleep still ages nothing (#2108).
    ///
    /// Only window reports are credited. A session's own `last_active` is not: a
    /// live one has a five-minute TTL that an outage rarely approaches, and an
    /// ended one's short linger is not worth stretching. Not a visible change, so
    /// it does not bump the change-notify.
    ///
    /// [`WorktreesRegistry::credit_outage`]: crate::worktrees::WorktreesRegistry::credit_outage
    pub fn credit_window_outage(&self, outage: Duration) {
        let awake = self.clock.now();
        for entry in self.lock_windows().values_mut() {
            // Saturate rather than overflow on an absurd credit.
            entry.last_active = entry
                .last_active
                .checked_add(outage)
                .map_or(awake, |credited| credited.min(awake));
        }
    }

    /// Reaps stale sessions and windows, then returns the live sessions with
    /// each [`Source`] resolved and sorted for deterministic output.
    ///
    /// Two independent locks, each held only for pure-CPU work and never
    /// nested: the sessions snapshot is taken and the lock dropped, then the
    /// windows snapshot, then the join runs lock-free. Path matching is a pure
    /// prefix compare (no canonicalization / disk I/O), honouring the
    /// `Mutex`-never-across-`.await` and no-I/O-under-lock invariants.
    ///
    /// Deliberately does **not** [`bump`](Self::bump), even when its inline reap
    /// drops an entry: this is the body of every subscription's `snapshot()`, so
    /// bumping here would feed the stream loop back into itself. A read-path reap
    /// reaches other subscribers on the server's next periodic re-sample, whose
    /// diff sees the shrunken list — the [`WorktreesRegistry::list`] arrangement.
    ///
    /// [`WorktreesRegistry::list`]: crate::worktrees::WorktreesRegistry::list
    pub fn list(&self) -> Vec<SessionEntry> {
        let awake = self.clock.now();
        let mut sessions: Vec<SessionEntry> = {
            let mut guard = self.lock_sessions();
            reap_sessions(&mut guard, self.session_ttl, self.ended_ttl, awake);
            guard.values().cloned().collect()
        };
        let windows: Vec<WindowEntry> = {
            let mut guard = self.lock_windows();
            reap_windows(&mut guard, self.window_ttl, awake);
            guard
                .values()
                .filter(|e| e.report.has_embedding())
                .cloned()
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
        let awake = self.clock.now();
        let mut windows = self.lock_windows();
        reap_windows(&mut windows, self.window_ttl, awake);
        pick_window(&cwd, windows.values().filter(|e| e.report.has_embedding()))
            .and_then(|w| w.folders.first().cloned())
    }
}

impl Default for SessionsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Records which agent process owns `entry` from a sighting's `pid` (#1948),
/// moving `pid_start` (#1916) in lockstep so the two are never out of sync.
///
/// A sighting from a pid other than the owner is a new process resuming the
/// session in place (every hook and the `claude-wrap` stream report the agent's
/// pid), so it takes ownership — along with whatever start-time token came with
/// it, which may be `None` if that sighting couldn't determine one — and the
/// old owner joins `replaced_pids`, whose late `end` is ignored. A replaced pid
/// never becomes the owner again, so a straggling hook from the old process
/// cannot take the session back. Not a rendered field, so it never decides a
/// [`bump`](SessionsRegistry::bump).
///
/// `observe` already filters out a sighting whose pid is in `replaced_pids`
/// before calling this, so the only two cases reaching here are the current
/// owner (a no-op) and a genuinely new one.
///
/// A new owner's `pid_start` (#1916) resets to `None`: the previous owner's
/// token describes a different process, and the [`pid_watcher`] captures the
/// new owner's own token independently the next time it confirms this pid
/// alive, rather than trusting anything client-supplied.
fn track_pid(entry: &mut SessionEntry, pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    if entry.pid == Some(pid) {
        return;
    }
    if let Some(owner) = entry.pid.replace(pid) {
        if entry.replaced_pids.len() >= MAX_REPLACED_PIDS {
            entry.replaced_pids.pop_front();
        }
        entry.replaced_pids.push_back(owner);
        // A different process now owns the session, and whether *it* is wrapped
        // is learned from its own next stream report (a terminal `--resume` of
        // a chat the extension once ran is not).
        entry.streamed = false;
    }
    entry.pid_start = None;
}

/// Fills `slot` from `incoming` only when `incoming` carries a value, so a
/// best-effort field never overwrites known data with `None` on a re-`observe`.
/// Returns whether the stored value actually changed, which is what decides
/// whether the sighting is worth a [`bump`](SessionsRegistry::bump) — a hook
/// re-sending the same `cwd` it sent last time is not.
fn fill<T: PartialEq>(slot: &mut Option<T>, incoming: Option<T>) -> bool {
    match incoming {
        Some(value) if slot.as_ref() != Some(&value) => {
            *slot = Some(value);
            true
        }
        _ => false,
    }
}

/// Resolves a session's [`Source`] by joining its `cwd` against the live
/// window-embedding reports.
///
/// Among the windows whose folder is a prefix of `cwd`, [`pick_window`] chooses
/// the winner. A session with no `cwd`, or no matching window, is
/// [`Source::Terminal`].
fn resolve_source(cwd: Option<&Path>, windows: &[WindowEntry]) -> Source {
    let Some(cwd) = cwd else {
        tracing::trace!(outcome = "missing_cwd", "session_attribution_miss");
        return Source::Terminal;
    };
    if let Some(window) = pick_window(cwd, windows) {
        Source::VsCode {
            window_key: window.key.clone(),
        }
    } else {
        tracing::debug!(cwd = %cwd.display(), window_count = windows.len(), "session_attribution_miss");
        for window in windows {
            tracing::trace!(window_key = %window.report.key, folders = ?window.report.folders, "session_attribution_candidate");
        }
        Source::Terminal
    }
}

/// Picks the window a session at `cwd` is attributed to, among the windows with
/// a folder that is a prefix of it: the longest matching folder (the most
/// specific root, so a later-opened parent-folder window does not outrank the
/// window the session actually runs in), then the most recently registered,
/// then the lowest key (#1451).
///
/// A window reload registers a fresh companion-generated key while the old
/// registration can live on for up to the window TTL, so the newest
/// registration is the live window. Ranking by key alone — the earlier rule —
/// picked whichever UUID sorted lowest. `registered_at` rather than
/// `last_seen`, because every open window refreshes `last_seen` on its own
/// heartbeat, so two live windows on one folder would alternate. The key tail
/// only keeps equal stamps deterministic. Shared by [`resolve_source`] and
/// [`SessionsRegistry::focus_folder`] so the tray focuses the window the tree
/// attributes the session to.
fn pick_window<'a>(
    cwd: &Path,
    windows: impl IntoIterator<Item = &'a WindowEntry>,
) -> Option<&'a WindowReport> {
    windows
        .into_iter()
        .filter_map(|e| Some((e, match_depth(cwd, &e.report.folders)?)))
        .min_by(|(a, a_depth), (b, b_depth)| {
            b_depth
                .cmp(a_depth)
                .then_with(|| b.registered_at.cmp(&a.registered_at))
                .then_with(|| a.report.key.cmp(&b.report.key))
        })
        .map(|(e, _)| &e.report)
}

/// The component count of the longest of `folders` that is a prefix of `cwd`,
/// or `None` when none is.
fn match_depth(cwd: &Path, folders: &[PathBuf]) -> Option<usize> {
    folders
        .iter()
        .filter(|f| cwd.starts_with(f))
        .map(|f| f.components().count())
        .max()
}

/// Whether `req` is a Claude session's authoritative stream-wrapper report —
/// the signal [`SessionEntry::streamed`] latches on. Codex's wrapper also emits
/// `StreamState`, but it polls an app-server it owns and re-asserts every poll,
/// and pi's extension carries no pid, so neither is affected.
fn is_claude_stream_report(req: &ObserveRequest) -> bool {
    req.agent == Agent::Claude && matches!(req.event, SessionEvent::StreamState(_))
}

/// Removes sessions last active longer than their TTL ago on the awake clock
/// (a shorter [`ended_ttl`](SessionsRegistry::ended_ttl) for `ended` sessions),
/// returning how many were dropped. `awake` is the registry's [`AwakeClock`]
/// reading, so time spent asleep never counts (#2108). Pure CPU; the caller
/// holds the sessions lock but never `.await`s under it.
///
/// This TTL is the fallback for a session the [`pid_watcher`] cannot vouch for
/// (no pid, an unconfirmed pid, or one that has never been prompted) — see
/// [`DEFAULT_SESSION_TTL`]. A pid the watcher has confirmed alive keeps this
/// function from ever seeing the entry go stale by refreshing `last_active`
/// itself, so no pid-specific logic belongs here: every liveness decision that
/// needs to inspect a process happens off this lock, in the watcher.
fn reap_sessions(
    sessions: &mut HashMap<String, SessionEntry>,
    session_ttl: Duration,
    ended_ttl: Duration,
    awake: Duration,
) -> usize {
    let before = sessions.len();
    sessions.retain(|_, e| {
        let max_age = if e.state == SessionState::Ended {
            ended_ttl
        } else {
            session_ttl
        };
        let keep = awake.saturating_sub(e.last_active) <= max_age;
        if !keep {
            tracing::trace!(session_id = %e.session_id, reason = if e.state == SessionState::Ended { "ended_ttl" } else { "session_ttl" }, "session_reaped");
        }
        keep
    });
    let count = before - sessions.len();
    if count > 0 {
        tracing::debug!(count, "sessions_reaped");
    }
    count
}

/// Removes window-embedding reports last refreshed longer than `ttl` ago on the
/// awake clock.
fn reap_windows(
    windows: &mut HashMap<String, WindowEntry>,
    ttl: Duration,
    awake: Duration,
) -> usize {
    let before = windows.len();
    windows.retain(|key, e| {
        let keep = awake.saturating_sub(e.last_active) <= ttl;
        if !keep {
            tracing::trace!(window_key = %key, reason = "window_ttl", "session_window_reaped");
        }
        keep
    });
    let count = before - windows.len();
    if count > 0 {
        tracing::debug!(count, "session_windows_reaped");
    }
    count
}

/// Removes the session with the oldest `last_active` (ties broken by lowest
/// `session_id` for determinism). Called when a new session would exceed
/// [`MAX_SESSIONS`].
fn evict_oldest_session(sessions: &mut HashMap<String, SessionEntry>) {
    let oldest = sessions
        .values()
        .min_by(|a, b| {
            a.last_active
                .cmp(&b.last_active)
                .then_with(|| a.session_id.cmp(&b.session_id))
        })
        .map(|e| e.session_id.clone());
    if let Some(key) = oldest {
        sessions.remove(&key);
        tracing::debug!(session_id = %key, reason = "capacity", "session_evicted");
    }
}

/// Removes the window report with the oldest `last_active` (ties broken by lowest
/// key). Called when a new window would exceed [`MAX_WINDOWS`].
fn evict_oldest_window(windows: &mut HashMap<String, WindowEntry>) {
    let oldest = windows
        .iter()
        .min_by(|a, b| {
            a.1.last_active
                .cmp(&b.1.last_active)
                .then_with(|| a.0.cmp(b.0))
        })
        .map(|(k, _)| k.clone());
    if let Some(key) = oldest {
        windows.remove(&key);
        tracing::debug!(window_key = %key, reason = "capacity", "session_window_evicted");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn registry_diagnostics_distinguish_transition_heartbeat_and_unknown_end() {
        let registry = SessionsRegistry::new();
        let logs = crate::test_support::capture_at(tracing::Level::DEBUG, || {
            registry.observe(observe_request(
                "diagnostics",
                SessionEvent::UserPromptSubmit,
                Some("/project"),
            ));
            registry.observe(observe_request(
                "diagnostics",
                SessionEvent::UserPromptSubmit,
                Some("/project"),
            ));
            registry.end("missing", None, None);
            registry.end("diagnostics", None, None);
        });
        assert!(logs.contains("state_changed") || logs.contains("created"));
        assert!(logs.contains("heartbeat_only"));
        assert!(logs.contains("unknown"));
        assert!(logs.contains("outcome=\"ended\""), "{logs}");
        assert!(logs.contains("bumped=true"));
        assert!(logs.contains("bumped=false"));
    }

    #[test]
    fn attribution_misses_are_logged_with_each_candidate_window() {
        let windows = vec![window_entry("w1", 20), window_entry("w2", 5)];
        let logs = crate::test_support::capture_at(tracing::Level::TRACE, || {
            assert_eq!(
                resolve_source(Some(Path::new("/elsewhere/x")), &windows),
                Source::Terminal
            );
            assert_eq!(resolve_source(None, &windows), Source::Terminal);
        });
        assert!(logs.contains("session_attribution_miss"), "{logs}");
        assert!(logs.contains("missing_cwd"), "{logs}");
        assert_eq!(
            logs.matches("session_attribution_candidate").count(),
            2,
            "{logs}"
        );
        assert!(
            logs.contains("window_key=w1") && logs.contains("window_key=w2"),
            "{logs}"
        );
    }

    fn observe_request(session_id: &str, event: SessionEvent, cwd: Option<&str>) -> ObserveRequest {
        ObserveRequest {
            agent_id: None,
            pid: None,
            agent: Agent::Claude,
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
            // An authoritative stream-json report is returned verbatim.
            (
                StreamState(SessionState::WaitingForPermission),
                SessionState::WaitingForPermission,
            ),
            (StreamState(SessionState::Idle), SessionState::Idle),
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
        // An authoritative StreamState overrides any current state — it read the
        // exact state from the stream rather than inferring it.
        assert_eq!(
            SessionState::for_event(
                &StreamState(SessionState::Idle),
                Some(SessionState::Working)
            ),
            SessionState::Idle
        );
        // Growth is expected while a session waits on the user (the transcript
        // grows before the prompt is answered) and around `SessionEnd` (the
        // final lines land as it exits), so in neither case is it evidence the
        // turn is running: the directly reported state stands (#1418). A
        // resumed session's old process writes to the shared transcript as it
        // exits, after the new one's `SessionStart` (#1946).
        for held in [
            SessionState::Starting,
            SessionState::WaitingForInput,
            SessionState::WaitingForPermission,
            SessionState::Ended,
        ] {
            assert_eq!(
                SessionState::for_event(&TranscriptGrew, Some(held)),
                held,
                "growth must not overwrite {held:?}"
            );
        }
        // From every other state growth still means working, as does growth on
        // a session whose state is not yet known (covered by the table above).
        for other in [SessionState::Working, SessionState::Idle] {
            assert_eq!(
                SessionState::for_event(&TranscriptGrew, Some(other)),
                SessionState::Working,
                "growth from {other:?}"
            );
        }
    }

    #[test]
    fn end_marks_ended_and_reaps_quickly() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/tmp/a"),
        ));
        assert!(reg.end("s1", Some("clear"), None));
        // Ending an unknown session is a no-op.
        assert!(!reg.end("ghost", None, None));
        let sessions = reg.list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, SessionState::Ended);
        // Let the ended entry sit awake past the short ended TTL: it reaps out.
        reg.clock.advance(Duration::from_secs(30));
        assert!(reg.list().is_empty(), "ended entry reaps after ended TTL");
    }

    // --- pid-based liveness (#1916): the registry side the pid_watcher drives ---

    #[test]
    fn pid_liveness_candidates_lists_only_non_ended_pid_bearing_sessions() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_from(
            "with_pid",
            SessionEvent::UserPromptSubmit,
            100,
        ));
        reg.observe(observe_request(
            "no_pid",
            SessionEvent::UserPromptSubmit,
            None,
        ));
        reg.observe(observe_from("ended", SessionEvent::UserPromptSubmit, 200));
        reg.end("ended", None, Some(200));

        let candidates = reg.pid_liveness_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "with_pid");
        assert_eq!(candidates[0].pid, 100);
        assert!(candidates[0].prompted);
        assert_eq!(candidates[0].pid_start, None);
    }

    #[test]
    fn only_a_claude_stream_report_marks_a_session_streamed() {
        // Hooks alone never mark it, even for a prompted session.
        let reg = SessionsRegistry::new();
        reg.observe(observe_from("hooked", SessionEvent::UserPromptSubmit, 100));
        let candidates = reg.pid_liveness_candidates();
        assert!(candidates[0].prompted);
        assert!(!candidates[0].streamed);

        // A Claude stream report does, whether it created the entry or arrived
        // later, and it is sticky across the hooks that follow.
        reg.observe(observe_from(
            "hooked",
            SessionEvent::StreamState(SessionState::Idle),
            100,
        ));
        reg.observe(observe_from("hooked", SessionEvent::Stop, 100));
        let candidates = reg.pid_liveness_candidates();
        assert!(candidates[0].prompted && candidates[0].streamed);

        let reg = SessionsRegistry::new();
        reg.observe(observe_from(
            "wrapped",
            SessionEvent::StreamState(SessionState::Working),
            100,
        ));
        assert!(reg.pid_liveness_candidates()[0].streamed);

        // A different process taking the session over (a terminal `--resume` of
        // a chat the extension once ran) is not known to be wrapped.
        let reg = SessionsRegistry::new();
        reg.observe(observe_from(
            "resumed",
            SessionEvent::StreamState(SessionState::Idle),
            100,
        ));
        assert!(reg.pid_liveness_candidates()[0].streamed);
        reg.observe(observe_from("resumed", SessionEvent::SessionStart, 200));
        assert!(!reg.pid_liveness_candidates()[0].streamed);

        // Another agent's `StreamState` (Codex's wrapper, pi's extension) is not
        // a Claude chat the extension is pinning.
        for agent in [Agent::Codex, Agent::Pi] {
            let reg = SessionsRegistry::new();
            reg.observe(ObserveRequest {
                agent,
                ..observe_from("other", SessionEvent::StreamState(SessionState::Idle), 100)
            });
            assert!(!reg.pid_liveness_candidates()[0].streamed, "{agent:?}");
        }
    }

    #[test]
    fn confirm_pid_liveness_refreshes_last_seen_and_captures_the_token_once() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_from("s1", SessionEvent::UserPromptSubmit, 100));
        let old = Utc::now() - chrono::Duration::seconds(1000);
        let old_active = reg.lock_sessions()["s1"].last_active;
        {
            let mut guard = reg.lock_sessions();
            guard.get_mut("s1").unwrap().last_seen = old;
        }
        reg.clock.advance(Duration::from_secs(1000));

        reg.confirm_pid_liveness("s1", Utc::now(), "tok-1");
        {
            let guard = reg.lock_sessions();
            let entry = &guard["s1"];
            assert!(entry.last_seen > old, "last_seen should be refreshed");
            assert!(
                entry.last_active > old_active + Duration::from_secs(999),
                "the awake stamp the TTL reads should be refreshed too"
            );
            assert_eq!(entry.pid_start.as_deref(), Some("tok-1"));
        }

        // A later confirmation does not overwrite an already-captured token —
        // it is fixed at the pid's first independently-confirmed sighting.
        reg.confirm_pid_liveness("s1", Utc::now(), "tok-2");
        assert_eq!(
            reg.lock_sessions()["s1"].pid_start.as_deref(),
            Some("tok-1")
        );
    }

    #[test]
    fn confirm_pid_liveness_is_a_noop_for_an_unknown_or_ended_session() {
        let reg = SessionsRegistry::new();
        // Unknown session: no panic.
        reg.confirm_pid_liveness("ghost", Utc::now(), "tok");

        reg.observe(observe_from("s1", SessionEvent::UserPromptSubmit, 100));
        reg.end("s1", None, Some(100));
        let before = reg.lock_sessions()["s1"].last_seen;
        let before_active = reg.lock_sessions()["s1"].last_active;
        reg.clock.advance(Duration::from_secs(1));
        reg.confirm_pid_liveness("s1", Utc::now(), "tok");
        assert_eq!(
            reg.lock_sessions()["s1"].last_seen,
            before,
            "an ended session must not be revived by a liveness confirmation"
        );
        assert_eq!(reg.lock_sessions()["s1"].last_active, before_active);
    }

    #[test]
    fn stale_working_session_reaps_but_recent_survives() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("stale", SessionEvent::PreToolUse, None));
        reg.clock.advance(Duration::from_secs(1000));
        reg.observe(observe_request("fresh", SessionEvent::PreToolUse, None));
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
        // Let the window report sit awake past the window TTL.
        reg.clock.advance(Duration::from_secs(120));
        assert_eq!(reg.list()[0].source, Source::Terminal);
    }

    #[test]
    fn credit_window_outage_keeps_a_report_the_outage_alone_would_have_expired() {
        let reg = SessionsRegistry::new();
        let report = |key: &str| WindowReport {
            key: key.to_string(),
            folders: vec![PathBuf::from("/p")],
            tabs: 1,
            terminals: 0,
        };
        reg.report_window(report("kept"));
        reg.report_window(report("long-gone"));
        // Room on the awake clock to back-date a stamp by minutes.
        reg.clock.advance(Duration::from_secs(1000));
        {
            let awake = reg.clock.now();
            let mut guard = reg.lock_windows();
            // 40 s silent is past the 30 s TTL, but 25 s of it was the outage.
            guard.get_mut("kept").unwrap().last_active =
                awake.saturating_sub(Duration::from_secs(40));
            guard.get_mut("long-gone").unwrap().last_active =
                awake.saturating_sub(Duration::from_secs(300));
        }
        // Any read reaps; the already-dead report goes before the credit lands.
        reg.credit_window_outage(Duration::from_secs(25));
        reg.list();
        let guard = reg.lock_windows();
        assert!(guard.contains_key("kept"), "the outage must not age it out");
        assert!(
            !guard.contains_key("long-gone"),
            "a report 300 s silent is dead however long the outage was"
        );
    }

    #[test]
    fn credit_window_outage_never_moves_last_active_into_the_future() {
        let reg = SessionsRegistry::new();
        reg.report_window(WindowReport {
            key: "w".to_string(),
            folders: vec![PathBuf::from("/p")],
            tabs: 1,
            terminals: 0,
        });
        reg.credit_window_outage(Duration::from_secs(3600));
        assert!(reg.lock_windows()["w"].last_active <= reg.clock.now());
        // A credit too large to add is saturated to now, not a panic.
        reg.credit_window_outage(Duration::MAX);
        assert!(reg.lock_windows()["w"].last_active <= reg.clock.now());
    }

    #[test]
    fn credit_window_outage_is_not_a_visible_change() {
        let reg = SessionsRegistry::new();
        reg.report_window(WindowReport {
            key: "w".to_string(),
            folders: vec![PathBuf::from("/p")],
            tabs: 1,
            terminals: 0,
        });
        let rx = reg.subscribe_changes();
        reg.credit_window_outage(Duration::from_secs(5));
        assert!(!rx.has_changed().unwrap());
    }

    /// A window entry covering `/p`, registered `age_secs` ago.
    fn window_entry(key: &str, age_secs: i64) -> WindowEntry {
        let now = Utc::now();
        WindowEntry {
            report: window_report(key, "/p", true),
            last_active: Duration::ZERO,
            registered_at: now - chrono::Duration::seconds(age_secs),
        }
    }

    fn vscode(key: &str) -> Source {
        Source::VsCode {
            window_key: key.to_string(),
        }
    }

    #[test]
    fn resolve_source_prefers_newest_registration_over_key_order() {
        // `w1` sorts lowest but registered earlier: the newer `w2` wins (#1451).
        let windows = vec![window_entry("w1", 20), window_entry("w2", 5)];
        assert_eq!(
            resolve_source(Some(Path::new("/p/x")), &windows),
            vscode("w2")
        );
        // No cwd → terminal.
        assert_eq!(resolve_source(None, &windows), Source::Terminal);
    }

    #[test]
    fn resolve_source_breaks_equal_registrations_by_lowest_key() {
        let now = Utc::now();
        let at_now = |key: &str| WindowEntry {
            report: window_report(key, "/p", true),
            last_active: Duration::ZERO,
            registered_at: now,
        };
        let windows = vec![at_now("w2"), at_now("w1")];
        assert_eq!(
            resolve_source(Some(Path::new("/p/x")), &windows),
            vscode("w1")
        );
    }

    #[test]
    fn reload_overlap_attributes_to_the_new_window_even_if_its_key_sorts_higher() {
        // The #1451 scenario: the old registration is still live inside its
        // TTL (and even still heartbeating) when the reloaded window registers.
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/p/sub"),
        ));
        reg.report_window(window_report("39-old", "/p", true));
        {
            let mut guard = reg.lock_windows();
            guard.get_mut("39-old").unwrap().registered_at =
                Utc::now() - chrono::Duration::seconds(60);
        }
        reg.report_window(window_report("52-new", "/p", true));
        assert_eq!(reg.list()[0].source, vscode("52-new"));
        // A heartbeat from the old window must not take the session back.
        reg.report_window(window_report("39-old", "/p", true));
        assert_eq!(reg.list()[0].source, vscode("52-new"));
    }

    #[test]
    fn a_more_specific_folder_outranks_a_newer_parent_window() {
        let now = Utc::now();
        let entry = |key: &str, folder: &str, age_secs: i64| WindowEntry {
            report: window_report(key, folder, true),
            last_active: Duration::ZERO,
            registered_at: now - chrono::Duration::seconds(age_secs),
        };
        // `parent` registered later and has the lower key, but `/repo/sub` is the
        // root the session actually runs in.
        let windows = vec![
            entry("a-parent", "/repo", 1),
            entry("z-sub", "/repo/sub", 50),
        ];
        assert_eq!(
            resolve_source(Some(Path::new("/repo/sub/x")), &windows),
            vscode("z-sub")
        );
        // A session outside `/repo/sub` still falls to the parent window.
        assert_eq!(
            resolve_source(Some(Path::new("/repo/other")), &windows),
            vscode("a-parent")
        );
    }

    #[test]
    fn refreshing_a_window_preserves_its_registration_time() {
        let reg = SessionsRegistry::new();
        reg.report_window(window_report("w1", "/p", true));
        let first = reg.lock_windows()["w1"].registered_at;
        let first_active = reg.lock_windows()["w1"].last_active;
        reg.clock.advance(Duration::from_secs(5));
        reg.report_window(window_report("w1", "/p", true));
        let guard = reg.lock_windows();
        assert_eq!(guard["w1"].registered_at, first);
        assert!(guard["w1"].last_active > first_active);
    }

    #[test]
    fn reregistering_after_unregister_gets_a_fresh_registration_time() {
        let reg = SessionsRegistry::new();
        reg.report_window(window_report("w1", "/p", true));
        let first = reg.lock_windows()["w1"].registered_at;
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(reg.unregister_window("w1"));
        reg.report_window(window_report("w1", "/p", true));
        assert!(reg.lock_windows()["w1"].registered_at > first);
    }

    #[test]
    fn focus_folder_agrees_with_list_on_overlap() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/p/sub"),
        ));
        let mut old = window_report("a-old", "/p", true);
        old.folders = vec![PathBuf::from("/p/old-root"), PathBuf::from("/p")];
        reg.report_window(old);
        {
            let mut guard = reg.lock_windows();
            guard.get_mut("a-old").unwrap().registered_at =
                Utc::now() - chrono::Duration::seconds(60);
        }
        reg.report_window(window_report("z-new", "/p", true));
        assert_eq!(reg.list()[0].source, vscode("z-new"));
        // Not the old window's first folder `/p/old-root`.
        assert_eq!(reg.focus_folder("s1"), Some(PathBuf::from("/p")));
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
        for (id, age) in [("young", 0_u64), ("old", 100), ("older", 200)] {
            sessions.insert(
                id.to_string(),
                SessionEntry {
                    subagent_base: None,
                    subagent_waits: HashMap::new(),
                    pid: None,
                    pid_start: None,
                    prompted: false,
                    streamed: false,
                    replaced_pids: VecDeque::new(),
                    recent_seqs: VecDeque::new(),
                    latest_stamp_ts: None,
                    agent: Agent::Claude,
                    session_id: id.to_string(),
                    cwd: None,
                    transcript_path: None,
                    repo: None,
                    model: None,
                    state: SessionState::Working,
                    source: Source::Terminal,
                    last_event: SessionEvent::PreToolUse,
                    started_at: now,
                    last_seen: now,
                    last_active: Duration::from_secs(1000 - age),
                },
            );
        }
        evict_oldest_session(&mut sessions);
        assert!(!sessions.contains_key("older"));
        assert!(sessions.contains_key("young"));
        assert!(sessions.contains_key("old"));
        // An empty map is a no-op, not a panic.
        let mut empty: HashMap<String, SessionEntry> = HashMap::new();
        evict_oldest_session(&mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn list_sorts_by_repo_then_session_id() {
        let reg = SessionsRegistry::new();
        for (id, repo) in [("z", "repo-a"), ("a", "repo-b"), ("m", "repo-a")] {
            reg.observe(ObserveRequest {
                agent_id: None,
                pid: None,
                agent: Agent::Claude,
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
            agent_id: None,
            pid: None,
            agent: Agent::Claude,
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
    fn stream_state_is_authoritative_and_round_trips() {
        // The `claude-wrap` wrapper reports the exact state; `observe` applies it
        // verbatim and overrides whatever was inferred before.
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("s1", SessionEvent::PreToolUse, Some("/p")));
        assert_eq!(reg.list()[0].state, SessionState::Working);
        reg.observe(observe_request(
            "s1",
            SessionEvent::StreamState(SessionState::WaitingForPermission),
            None,
        ));
        assert_eq!(reg.list()[0].state, SessionState::WaitingForPermission);
        // The wire shape is a nested tuple variant, matching `{"notification":…}`.
        let value = serde_json::to_value(SessionEvent::StreamState(
            SessionState::WaitingForPermission,
        ))
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "stream_state": "waiting_for_permission" })
        );
        // …and deserializes back.
        let event: SessionEvent = serde_json::from_value(value).unwrap();
        assert_eq!(
            event,
            SessionEvent::StreamState(SessionState::WaitingForPermission)
        );
    }

    #[test]
    fn default_constructs_an_empty_registry() {
        let reg = SessionsRegistry::default();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn agent_is_omitted_for_claude_and_tagged_for_pi_on_the_wire() {
        // Claude senders stay byte-identical: no `agent` key.
        let claude = serde_json::to_value(observe_request("s", SessionEvent::Stop, None)).unwrap();
        assert!(claude.get("agent").is_none(), "{claude}");
        // An absent `agent` reads back as Claude.
        let parsed: ObserveRequest =
            serde_json::from_value(serde_json::json!({ "session_id": "s", "event": "stop" }))
                .unwrap();
        assert_eq!(parsed.agent, Agent::Claude);
        // The exact payload the pi extension sends.
        let pi: ObserveRequest = serde_json::from_value(serde_json::json!({
            "session_id": "019a0000-0000-7000-8000-000000000000",
            "cwd": "/work/repo",
            "agent": "pi",
            "event": { "stream_state": "waiting_for_input" },
        }))
        .unwrap();
        assert_eq!(pi.agent, Agent::Pi);
        assert_eq!(
            pi.event,
            SessionEvent::StreamState(SessionState::WaitingForInput)
        );
    }

    #[test]
    fn agents_have_display_names() {
        assert_eq!(Agent::Claude.display_name(), "Claude");
        assert_eq!(Agent::Pi.display_name(), "pi");
        assert_eq!(Agent::Codex.display_name(), "Codex");
    }

    #[test]
    fn an_ended_session_is_not_refreshed_by_a_passive_sighting_or_a_second_end() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("s", SessionEvent::Stop, None));
        assert!(reg.end("s", None, None));
        let ended_at = reg.list()[0].last_seen;
        let mut changes = reg.subscribe_changes();
        changes.mark_unchanged();
        std::thread::sleep(std::time::Duration::from_millis(5));
        reg.observe(observe_request(
            "s",
            SessionEvent::TranscriptDiscovered,
            None,
        ));
        assert!(reg.end("s", None, None), "still a known session");
        let listed = reg.list();
        assert_eq!(listed[0].state, SessionState::Ended);
        assert_eq!(
            listed[0].last_seen, ended_at,
            "the linger window did not restart"
        );
        assert!(
            !changes.has_changed().unwrap(),
            "no consumer-visible change"
        );
        // A hook event still revives it, as before.
        reg.observe(observe_request("s", SessionEvent::UserPromptSubmit, None));
        assert_eq!(reg.list()[0].state, SessionState::Working);
    }

    #[test]
    fn codex_is_a_tagged_agent_on_the_wire() {
        let codex: ObserveRequest = serde_json::from_value(serde_json::json!({
            "session_id": "019a0000-0000-7000-8000-000000000001",
            "agent": "codex",
            "event": { "notification": "permission_prompt" },
        }))
        .unwrap();
        assert_eq!(codex.agent, Agent::Codex);
        assert!(!codex.agent.is_claude());
        let json = serde_json::to_value(&codex).unwrap();
        assert_eq!(json["agent"], "codex");
    }

    #[test]
    fn a_pi_session_is_listed_with_its_agent_and_reported_state() {
        let reg = SessionsRegistry::new();
        let mut req = observe_request(
            "pi-1",
            SessionEvent::StreamState(SessionState::Working),
            Some("/work/repo"),
        );
        req.agent = Agent::Pi;
        reg.observe(req);
        // A later sighting without the tag cannot re-label the session.
        reg.observe(observe_request(
            "pi-1",
            SessionEvent::StreamState(SessionState::Idle),
            None,
        ));
        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].agent, Agent::Pi);
        assert_eq!(listed[0].state, SessionState::Idle);
        let json = serde_json::to_value(&listed[0]).unwrap();
        assert_eq!(json["agent"], "pi");
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
                        subagent_base: None,
                        subagent_waits: HashMap::new(),
                        pid: None,
                        pid_start: None,
                        prompted: false,
                        streamed: false,
                        replaced_pids: VecDeque::new(),
                        recent_seqs: VecDeque::new(),
                        latest_stamp_ts: None,
                        agent: Agent::Claude,
                        session_id: id.clone(),
                        cwd: None,
                        transcript_path: None,
                        repo: None,
                        model: None,
                        state: SessionState::Working,
                        source: Source::Terminal,
                        last_event: SessionEvent::PreToolUse,
                        started_at: base,
                        last_seen: base,
                        last_active: Duration::from_millis(1_000_000 - i as u64),
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
                        last_active: Duration::from_millis(1_000_000 - i as u64),
                        registered_at: base,
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
        let at = |key: &str, secs: u64| WindowEntry {
            report: WindowReport {
                key: key.to_string(),
                folders: vec![],
                tabs: 1,
                terminals: 0,
            },
            last_active: Duration::from_secs(100 - secs),
            registered_at: now,
        };
        windows.insert("young".to_string(), at("young", 0));
        windows.insert("old-b".to_string(), at("old-b", 10));
        windows.insert("old-a".to_string(), at("old-a", 10));
        // Oldest `last_active` is shared; the lowest key loses.
        evict_oldest_window(&mut windows);
        assert!(!windows.contains_key("old-a"));
        assert!(windows.contains_key("old-b"));
        assert!(windows.contains_key("young"));
        // An empty map is a no-op, not a panic.
        let mut empty: HashMap<String, WindowEntry> = HashMap::new();
        evict_oldest_window(&mut empty);
        assert!(empty.is_empty());
    }

    // --- Change-notify for the push subscription (#1414) --------------------

    /// A window report with one folder, parameterized by whether it embeds Claude.
    fn window_report(key: &str, folder: &str, embedded: bool) -> WindowReport {
        WindowReport {
            key: key.to_string(),
            folders: vec![PathBuf::from(folder)],
            tabs: usize::from(embedded),
            terminals: 0,
        }
    }

    #[test]
    fn subscribe_changes_starts_seen_and_a_new_session_bumps() {
        let reg = SessionsRegistry::new();
        let mut rx = reg.subscribe_changes();
        // A fresh receiver has the current version already marked seen.
        assert!(!rx.has_changed().unwrap());
        reg.observe(observe_request("s1", SessionEvent::SessionStart, None));
        assert!(rx.has_changed().unwrap(), "a new session should bump");
        // Marking it seen clears the pending change.
        rx.borrow_and_update();
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn observe_bumps_on_a_state_transition_but_not_on_a_repeat_sighting() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/tmp/a"),
        ));
        // Subscribe *after* the insert so its bump is already seen.
        let mut rx = reg.subscribe_changes();

        // Same event, same cwd: liveness and `last_event`/`last_seen` move, but
        // nothing a consumer renders does. Hooks fire on every tool call, so this
        // is the hot path that must stay quiet.
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/tmp/a"),
        ));
        assert!(
            !rx.has_changed().unwrap(),
            "a repeat sighting with no visible change must not bump"
        );

        // `PreToolUse` → `Stop` flips working → idle, which the tree renders.
        reg.observe(observe_request("s1", SessionEvent::Stop, None));
        assert!(rx.has_changed().unwrap(), "a state transition should bump");
        rx.borrow_and_update();

        // A best-effort field taking a *new* value is visible too (the tally
        // joins sessions to worktree rows by `cwd`).
        reg.observe(observe_request("s1", SessionEvent::Stop, Some("/tmp/b")));
        assert!(
            rx.has_changed().unwrap(),
            "a newly-filled `cwd` should bump"
        );
    }

    #[test]
    fn transcript_growth_does_not_clobber_a_waiting_session() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::Notification(NotificationKind::PermissionPrompt),
            Some("/tmp/a"),
        ));
        // Subscribe *after* the insert so its bump is already seen. Never marked
        // seen below, so the closing assert catches the release's bump only if
        // the growth in between really did stay quiet.
        let rx = reg.subscribe_changes();

        // The watcher sees the assistant `tool_use` line Claude flushed before
        // the prompt could be answered. The wait came from a direct
        // `Notification`, so it must survive (#1418).
        reg.observe(observe_request(
            "s1",
            SessionEvent::TranscriptGrew,
            Some("/tmp/a"),
        ));
        assert_eq!(reg.list()[0].state, SessionState::WaitingForPermission);
        assert!(
            !rx.has_changed().unwrap(),
            "state did not change, so nothing a consumer renders did either (#1414)"
        );

        // Answering the prompt still releases the wait — on the next hook, which
        // for an approved tool is the `PostToolUse` that fires when it finishes.
        reg.observe(observe_request("s1", SessionEvent::PostToolUse, None));
        assert_eq!(reg.list()[0].state, SessionState::Working);
        assert!(
            rx.has_changed().unwrap(),
            "the release is a real transition"
        );
    }

    #[test]
    fn transcript_growth_does_not_revive_an_ended_session() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::PreToolUse,
            Some("/tmp/a"),
        ));
        assert!(reg.end("s1", Some("clear"), None));
        // Subscribed after the end so its bump is already seen (see above).
        let rx = reg.subscribe_changes();

        // The watcher's next scan sees the lines Claude flushed as it exited.
        // `SessionEnd` reported the end directly, so the entry must stay `ended`
        // and reap on the short ended TTL rather than being revived as a
        // `working` phantom that outlives the session by the whole TTL (#1418).
        reg.observe(observe_request(
            "s1",
            SessionEvent::TranscriptGrew,
            Some("/tmp/a"),
        ));
        assert_eq!(reg.list()[0].state, SessionState::Ended);
        assert!(
            !rx.has_changed().unwrap(),
            "state did not change, so nothing a consumer renders did either (#1414)"
        );
    }

    #[test]
    fn a_resumed_session_stays_starting_until_its_first_prompt() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request(
            "s1",
            SessionEvent::UserPromptSubmit,
            Some("/tmp/a"),
        ));
        // A VS Code window reload: the old process fires `SessionEnd`, and the
        // new one resumes the same session id and fires `SessionStart`.
        assert!(reg.end("s1", Some("other"), None));
        reg.observe(observe_request(
            "s1",
            SessionEvent::SessionStart,
            Some("/tmp/a"),
        ));
        assert_eq!(reg.list()[0].state, SessionState::Starting);

        // The watcher's next scan sees the `cost-state` line the old process
        // appended as it exited. It is not the resumed session doing work, and
        // no hook fires while it sits unprompted, so it must not read as
        // `working` (#1946).
        reg.observe(observe_request(
            "s1",
            SessionEvent::TranscriptGrew,
            Some("/tmp/a"),
        ));
        assert_eq!(reg.list()[0].state, SessionState::Starting);

        // The first prompt still moves it to `working`.
        reg.observe(observe_request("s1", SessionEvent::UserPromptSubmit, None));
        assert_eq!(reg.list()[0].state, SessionState::Working);
    }

    #[test]
    fn end_bumps_only_for_a_known_session() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("s1", SessionEvent::PreToolUse, None));
        let mut rx = reg.subscribe_changes();

        assert!(
            !reg.end("ghost", None, None),
            "an unknown session is a no-op"
        );
        assert!(
            !rx.has_changed().unwrap(),
            "ending an unknown session must not bump"
        );

        assert!(reg.end("s1", None, None));
        assert!(rx.has_changed().unwrap(), "a real end should bump");
        rx.borrow_and_update();
    }

    /// A hook sighting from agent process `pid`.
    fn observe_from(session_id: &str, event: SessionEvent, pid: u32) -> ObserveRequest {
        ObserveRequest {
            agent_id: None,
            pid: Some(pid),
            ..observe_request(session_id, event, None)
        }
    }

    #[test]
    fn a_late_end_from_the_replaced_process_keeps_a_resumed_session_live() {
        let reg = SessionsRegistry::new();
        // The old process (pid 100) runs the session; a window reload starts
        // the new one (pid 200) on the same session_id before 100 has exited.
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 100));
        reg.observe(observe_from("s1", SessionEvent::Stop, 100));
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 200));
        let rx = reg.subscribe_changes();

        // The old process's `SessionEnd` lands last (#1948).
        assert!(reg.end("s1", Some("other"), Some(100)));
        assert!(!rx.has_changed().unwrap(), "an ignored end must not bump");
        let sessions = reg.list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, SessionState::Starting);

        // The new process's own `SessionEnd` still ends it.
        assert!(reg.end("s1", Some("exit"), Some(200)));
        assert_eq!(reg.list()[0].state, SessionState::Ended);
    }

    #[test]
    fn a_straggling_hook_from_the_replaced_process_does_not_take_the_session_back() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 100));
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 200));
        // A replaced pid never becomes the owner again, so 200 still owns it —
        // and the straggling `PostToolUse` must not move the state either
        // (`observe`, not only `end`, ignores a replaced pid's sighting).
        reg.observe(observe_from("s1", SessionEvent::PostToolUse, 100));
        assert_eq!(reg.list()[0].state, SessionState::Starting);
        reg.end("s1", None, Some(100));
        assert_ne!(reg.list()[0].state, SessionState::Ended);
    }

    #[test]
    fn a_straggling_non_end_hook_from_the_replaced_process_does_not_change_state() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_from("s1", SessionEvent::UserPromptSubmit, 100));
        assert_eq!(reg.list()[0].state, SessionState::Working);
        // The resume: a new process takes over ownership.
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 200));
        assert_eq!(reg.list()[0].state, SessionState::Starting);
        let rx = reg.subscribe_changes();

        // The old process's last, delayed `Stop` arrives after the resume.
        // Left unguarded, `for_event(Stop, Starting)` would move this back to
        // `idle`, corrupting the resumed session's freshly-`starting` state.
        reg.observe(observe_from("s1", SessionEvent::Stop, 100));
        assert_eq!(reg.list()[0].state, SessionState::Starting);
        assert!(
            !rx.has_changed().unwrap(),
            "an ignored sighting must not bump"
        );
    }

    #[test]
    fn an_end_not_from_the_replaced_process_still_ends_the_session() {
        // No pid (an older sink, or a feed that sends none), the owning pid, and
        // a never-seen pid (a wrapped hook command whose parent is a per-hook
        // shell) all end the session: only a replaced pid is ignored.
        for end_pid in [None, Some(200), Some(999)] {
            let reg = SessionsRegistry::new();
            reg.observe(observe_from("s1", SessionEvent::SessionStart, 100));
            reg.observe(observe_from("s1", SessionEvent::SessionStart, 200));
            assert!(reg.end("s1", None, end_pid));
            assert_eq!(
                reg.list()[0].state,
                SessionState::Ended,
                "end with pid {end_pid:?}"
            );
        }
        // A session that was never resumed ends on its own process's end.
        let reg = SessionsRegistry::new();
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 100));
        assert!(reg.end("s1", None, Some(100)));
        assert_eq!(reg.list()[0].state, SessionState::Ended);
    }

    #[test]
    fn every_process_of_a_reload_burst_is_remembered_as_replaced() {
        let reg = SessionsRegistry::new();
        for pid in [100, 200, 300] {
            reg.observe(observe_from("s1", SessionEvent::SessionStart, pid));
        }
        // Both earlier processes' ends land after the third process started.
        assert!(reg.end("s1", None, Some(100)));
        assert!(reg.end("s1", None, Some(200)));
        assert_eq!(reg.list()[0].state, SessionState::Starting);
    }

    #[test]
    fn a_stream_report_from_a_new_process_takes_the_session_over() {
        // A wrapper-only install: `claude-wrap` reports `StreamState` with the
        // child's pid, and no hook ever sends a `SessionStart`.
        let idle = || SessionEvent::StreamState(SessionState::Idle);
        let reg = SessionsRegistry::new();
        reg.observe(observe_from("s1", idle(), 100));
        reg.observe(observe_from("s1", idle(), 200));
        reg.end("s1", None, Some(100));
        assert_eq!(reg.list()[0].state, SessionState::Idle);
    }

    #[test]
    fn replaced_pids_are_capped_oldest_first() {
        let reg = SessionsRegistry::new();
        let last = u32::try_from(MAX_REPLACED_PIDS).unwrap() + 1;
        for pid in 0..=last {
            reg.observe(observe_from("s1", SessionEvent::PostToolUse, pid));
        }
        let guard = reg.lock_sessions();
        let replaced = &guard["s1"].replaced_pids;
        assert_eq!(replaced.len(), MAX_REPLACED_PIDS);
        assert_eq!(replaced.front(), Some(&1), "pid 0 was forgotten first");
        assert_eq!(replaced.back(), Some(&(last - 1)));
    }

    #[test]
    fn a_sessions_first_pid_sighting_is_not_a_replacement() {
        let reg = SessionsRegistry::new();
        // Some feeds (e.g. the transcript watcher) can create an entry with no
        // pid at all; its first sighting of one has no prior owner to replace.
        reg.observe(observe_request(
            "s1",
            SessionEvent::TranscriptDiscovered,
            None,
        ));
        reg.observe(observe_from("s1", SessionEvent::PostToolUse, 100));
        let guard = reg.lock_sessions();
        assert_eq!(guard["s1"].pid, Some(100));
        assert!(guard["s1"].replaced_pids.is_empty());
    }

    #[test]
    fn end_then_start_resume_ordering_is_unchanged() {
        // #1946's ordering: the old process ends first, then the new one starts.
        let reg = SessionsRegistry::new();
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 100));
        assert!(reg.end("s1", None, Some(100)));
        reg.observe(observe_from("s1", SessionEvent::SessionStart, 200));
        assert_eq!(reg.list()[0].state, SessionState::Starting);
    }

    #[test]
    fn window_report_bumps_only_when_it_changes_the_source_join() {
        let reg = SessionsRegistry::new();
        reg.report_window(window_report("w1", "/p", true));
        let mut rx = reg.subscribe_changes();

        // The unchanged ~10s refresh every open window sends: liveness only.
        reg.report_window(window_report("w1", "/p", true));
        assert!(
            !rx.has_changed().unwrap(),
            "an unchanged window refresh must not bump"
        );

        // The window's Claude tab closed → its sessions fall back to `terminal`.
        reg.report_window(window_report("w1", "/p", false));
        assert!(
            rx.has_changed().unwrap(),
            "an embedding that vanished should bump"
        );
        rx.borrow_and_update();

        // Different folders → a different `cwd`-prefix join.
        reg.report_window(window_report("w1", "/q", false));
        assert!(rx.has_changed().unwrap(), "changed folders should bump");
        rx.borrow_and_update();

        // A brand-new window joins the registry.
        reg.report_window(window_report("w2", "/r", true));
        assert!(rx.has_changed().unwrap(), "a new window should bump");
    }

    #[test]
    fn unregister_window_bumps_only_when_it_removes() {
        let reg = SessionsRegistry::new();
        reg.report_window(window_report("w1", "/p", true));
        let rx = reg.subscribe_changes();

        assert!(!reg.unregister_window("ghost"));
        assert!(
            !rx.has_changed().unwrap(),
            "a no-op unregister must not bump"
        );

        assert!(reg.unregister_window("w1"));
        assert!(
            rx.has_changed().unwrap(),
            "a removing unregister should bump"
        );
    }

    #[tokio::test]
    async fn a_burst_of_bumps_coalesces_into_one_wakeup() {
        let reg = SessionsRegistry::new();
        let mut rx = reg.subscribe_changes();
        // Three visible changes back to back, all before anyone awaits.
        reg.observe(observe_request("s1", SessionEvent::SessionStart, None));
        reg.observe(observe_request("s2", SessionEvent::SessionStart, None));
        reg.observe(observe_request("s3", SessionEvent::SessionStart, None));
        // `changed()` marks the newest version seen, so the burst is one wakeup…
        rx.changed().await.unwrap();
        // …and there is nothing left pending for a second one.
        assert!(
            !rx.has_changed().unwrap(),
            "a burst should collapse into a single wakeup"
        );
    }

    #[test]
    fn list_does_not_bump_even_when_it_reaps() {
        // `list` is the body of every subscription's `snapshot()`, so a bump here
        // would feed the stream loop back into itself. A read-path reap reaches
        // other subscribers on the server's next periodic re-sample instead.
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("s1", SessionEvent::PreToolUse, None));
        // Let the entry sit awake past its TTL so the next `list` reaps it.
        reg.clock.advance(Duration::from_secs(600));
        let rx = reg.subscribe_changes();
        assert!(reg.list().is_empty(), "the stale session should be reaped");
        assert!(!rx.has_changed().unwrap(), "`list` must never bump");
    }

    // --- Sleep-safe TTLs (#2108) ---------------------------------------------

    /// A hair under the session TTL, still inside it.
    fn just_inside_the_ttl() -> Duration {
        DEFAULT_SESSION_TTL
            .checked_sub(Duration::from_secs(1))
            .unwrap()
    }

    /// Moves every wall-clock stamp back by `by`, as a laptop sleeping that long
    /// would, without any awake time passing.
    fn jump_wall_clock_forward(reg: &SessionsRegistry, by: chrono::Duration) {
        for entry in reg.lock_sessions().values_mut() {
            entry.last_seen -= by;
        }
    }

    #[test]
    fn a_wall_clock_jump_with_no_awake_time_reaps_nothing() {
        // The 2026-10-03 incident: a 422s sleep (and here, far longer) used to
        // reap every live session on the first read after the wake-up.
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("working", SessionEvent::PreToolUse, None));
        reg.observe(observe_request("idle", SessionEvent::Stop, None));
        reg.observe(observe_request("ended", SessionEvent::PreToolUse, None));
        reg.end("ended", None, None);
        reg.report_window(window_report("w1", "/p", true));

        jump_wall_clock_forward(&reg, chrono::Duration::days(3));

        let ids: Vec<String> = reg.list().into_iter().map(|s| s.session_id).collect();
        assert_eq!(
            ids,
            vec![
                "ended".to_string(),
                "idle".to_string(),
                "working".to_string()
            ],
            "sleeping must not age a session, not even an ended one"
        );
        assert_eq!(reg.lock_windows().len(), 1, "nor a window report");
        // Display still reads the wall clock: the stale `last_seen` is shown.
        assert!(reg.list()[0].last_seen < Utc::now() - chrono::Duration::days(2));
    }

    #[test]
    fn awake_time_past_the_ttl_still_reaps() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("s1", SessionEvent::PreToolUse, None));
        reg.report_window(window_report("w1", "/p", true));
        reg.clock.advance(just_inside_the_ttl());
        assert_eq!(reg.list().len(), 1, "inside the TTL it survives");
        assert_eq!(reg.lock_windows().len(), 0, "the window TTL is shorter");
        reg.clock.advance(Duration::from_secs(2));
        assert!(
            reg.list().is_empty(),
            "past the TTL of awake silence it reaps"
        );
    }

    #[test]
    fn activity_after_a_sleep_restarts_the_awake_ttl() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("s1", SessionEvent::PreToolUse, None));
        reg.clock.advance(just_inside_the_ttl());
        jump_wall_clock_forward(&reg, chrono::Duration::hours(8));
        // A hook after the wake-up refreshes the awake stamp as well.
        reg.observe(observe_request("s1", SessionEvent::PostToolUse, None));
        reg.clock.advance(just_inside_the_ttl());
        assert_eq!(reg.list().len(), 1);
    }

    // --- Stamped events: dedupe and ordering across socket and journal (#2108) --

    fn at(ts: &str) -> DateTime<Utc> {
        ts.parse().unwrap()
    }

    fn stamp(ts: &str, seq: &str) -> EventStamp {
        EventStamp {
            ts: at(ts),
            seq: seq.to_string(),
        }
    }

    fn state_of(reg: &SessionsRegistry, id: &str) -> SessionState {
        reg.lock_sessions()[id].state
    }

    #[test]
    fn the_second_copy_of_a_stamped_event_is_dropped_whatever_its_route() {
        let reg = SessionsRegistry::new();
        let work = stamp("2026-10-03T03:40:00Z", "1-a");
        let stop = stamp("2026-10-03T03:40:05Z", "1-b");
        reg.observe_stamped(
            observe_request("s", SessionEvent::PreToolUse, Some("/p")),
            Some(work.clone()),
            Origin::Socket,
        );
        reg.observe_stamped(
            observe_request("s", SessionEvent::Stop, None),
            Some(stop),
            Origin::Socket,
        );
        assert_eq!(state_of(&reg, "s"), SessionState::Idle);
        // The journal then hands over the working event again, and so would a
        // retried POST: neither may undo the `Stop`.
        for origin in [Origin::Journal, Origin::Socket] {
            reg.observe_stamped(
                observe_request("s", SessionEvent::PreToolUse, None),
                Some(work.clone()),
                origin,
            );
            assert_eq!(state_of(&reg, "s"), SessionState::Idle, "{origin:?}");
        }
    }

    #[test]
    fn a_journal_event_older_than_one_already_applied_is_stale() {
        let reg = SessionsRegistry::new();
        reg.observe_stamped(
            observe_request("s", SessionEvent::Stop, Some("/p")),
            Some(stamp("2026-10-03T03:40:05Z", "1-b")),
            Origin::Socket,
        );
        // A `PreToolUse` whose POST was dropped, read from the journal late.
        reg.observe_stamped(
            observe_request("s", SessionEvent::PreToolUse, None),
            Some(stamp("2026-10-03T03:40:00Z", "1-a")),
            Origin::Journal,
        );
        assert_eq!(state_of(&reg, "s"), SessionState::Idle);
        // A newer journal event is applied.
        reg.observe_stamped(
            observe_request("s", SessionEvent::PreToolUse, None),
            Some(stamp("2026-10-03T03:41:00Z", "1-c")),
            Origin::Journal,
        );
        assert_eq!(state_of(&reg, "s"), SessionState::Working);
    }

    #[test]
    fn a_socket_event_is_never_dropped_for_its_age() {
        // Concurrent hooks can reach the socket out of order (as ever), and a
        // wall clock that stepped back must not freeze a session.
        let reg = SessionsRegistry::new();
        reg.observe_stamped(
            observe_request("s", SessionEvent::Stop, Some("/p")),
            Some(stamp("2026-10-03T03:40:05Z", "1-b")),
            Origin::Socket,
        );
        reg.observe_stamped(
            observe_request("s", SessionEvent::PreToolUse, None),
            Some(stamp("2026-10-03T03:30:00Z", "1-a")),
            Origin::Socket,
        );
        assert_eq!(state_of(&reg, "s"), SessionState::Working);
    }

    #[test]
    fn unstamped_events_are_never_skipped() {
        let reg = SessionsRegistry::new();
        reg.observe(observe_request("s", SessionEvent::Stop, Some("/p")));
        reg.observe(observe_request("s", SessionEvent::PreToolUse, None));
        assert_eq!(state_of(&reg, "s"), SessionState::Working);
        reg.observe(observe_request("s", SessionEvent::Stop, None));
        assert_eq!(state_of(&reg, "s"), SessionState::Idle);
    }

    #[test]
    fn a_journal_end_older_than_a_resume_does_not_end_the_resumed_session() {
        let reg = SessionsRegistry::new();
        reg.observe_stamped(
            observe_request("s", SessionEvent::SessionStart, Some("/p")),
            Some(stamp("2026-10-03T03:40:00Z", "1-a")),
            Origin::Socket,
        );
        // `/clear`: the end was journaled at 03:41, but the resumed session's
        // own start (posted fine) is at 03:42.
        reg.observe_stamped(
            observe_request("s", SessionEvent::SessionStart, None),
            Some(stamp("2026-10-03T03:42:00Z", "1-c")),
            Origin::Socket,
        );
        let ended = reg.end_stamped(
            "s",
            Some("clear"),
            None,
            Some(stamp("2026-10-03T03:41:00Z", "1-b")),
            Origin::Journal,
        );
        assert!(ended, "the session is known");
        assert_eq!(state_of(&reg, "s"), SessionState::Starting);
        // The same end, newer than everything applied, does end it...
        reg.end_stamped(
            "s",
            None,
            None,
            Some(stamp("2026-10-03T03:43:00Z", "1-d")),
            Origin::Journal,
        );
        assert_eq!(state_of(&reg, "s"), SessionState::Ended);
        // ...once, and a duplicate of it is dropped.
        let ended_at = reg.lock_sessions()["s"].last_seen;
        reg.end_stamped(
            "s",
            None,
            None,
            Some(stamp("2026-10-03T03:43:00Z", "1-d")),
            Origin::Socket,
        );
        assert_eq!(reg.lock_sessions()["s"].last_seen, ended_at);
    }

    #[test]
    fn a_journal_event_keeps_its_own_timestamp_but_not_its_own_age() {
        let reg = SessionsRegistry::new();
        let long_ago = Utc::now() - chrono::Duration::hours(2);
        let st = EventStamp {
            ts: long_ago,
            seq: "1-a".to_string(),
        };
        reg.observe_stamped(
            observe_request("s", SessionEvent::UserPromptSubmit, Some("/p")),
            Some(st),
            Origin::Journal,
        );
        let listed = reg.list();
        assert_eq!(listed.len(), 1, "replay is not reaped on arrival");
        assert_eq!(listed[0].last_seen, long_ago, "history must not look fresh");
        assert_eq!(listed[0].started_at, long_ago);
        // Its TTL runs from now, in awake time.
        reg.clock.advance(just_inside_the_ttl());
        assert_eq!(reg.list().len(), 1);
        reg.clock.advance(Duration::from_secs(2));
        assert!(reg.list().is_empty());
    }

    #[test]
    fn a_journal_timestamp_in_the_future_is_clamped_to_now() {
        let reg = SessionsRegistry::new();
        let future = Utc::now() + chrono::Duration::hours(1);
        reg.observe_stamped(
            observe_request("s", SessionEvent::Stop, Some("/p")),
            Some(EventStamp {
                ts: future,
                seq: "1-a".to_string(),
            }),
            Origin::Journal,
        );
        assert!(reg.list()[0].last_seen <= Utc::now());
    }

    #[test]
    fn only_the_newest_seqs_are_remembered() {
        let reg = SessionsRegistry::new();
        for i in 0..=MAX_RECENT_SEQS {
            reg.observe_stamped(
                observe_request("s", SessionEvent::PreToolUse, Some("/p")),
                Some(stamp("2026-10-03T03:40:00Z", &format!("1-{i}"))),
                Origin::Socket,
            );
        }
        let guard = reg.lock_sessions();
        assert_eq!(guard["s"].recent_seqs.len(), MAX_RECENT_SEQS);
        assert!(!guard["s"].recent_seqs.contains(&"1-0".to_string()));
        assert!(guard["s"]
            .recent_seqs
            .contains(&format!("1-{MAX_RECENT_SEQS}")));
    }

    #[test]
    fn a_duplicate_does_not_bump_and_does_not_refresh_liveness() {
        let reg = SessionsRegistry::new();
        let st = stamp("2026-10-03T03:40:00Z", "1-a");
        reg.observe_stamped(
            observe_request("s", SessionEvent::PreToolUse, Some("/p")),
            Some(st.clone()),
            Origin::Socket,
        );
        let active = reg.lock_sessions()["s"].last_active;
        let rx = reg.subscribe_changes();
        reg.clock.advance(Duration::from_secs(10));
        reg.observe_stamped(
            observe_request("s", SessionEvent::PreToolUse, Some("/p")),
            Some(st),
            Origin::Journal,
        );
        assert!(!rx.has_changed().unwrap());
        assert_eq!(reg.lock_sessions()["s"].last_active, active);
    }
}
