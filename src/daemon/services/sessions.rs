//! The Claude Code sessions daemon service.
//!
//! A thin adapter that hosts the cross-window [`SessionsRegistry`] under the
//! daemon's lifecycle and exposes the ingest ops (`observe`/`end`/`window`/
//! `window-unregister`) and the read op (`list`) over the control socket, plus a
//! tray submenu with a per-session "focus" action for sessions embedded in a VS
//! Code window.
//!
//! All registry state and liveness logic (the two `Mutex<HashMap>`s, TTL
//! reaping, the entry caps, the `Source` join) lives in [`crate::sessions`]; this
//! adapter only routes ops, enriches a session's `repo` from its `cwd` via
//! `git2` (the disk I/O the engine deliberately avoids, off the registry lock on
//! a blocking thread — the worktrees `git_status` precedent, #1186), renders the
//! menu/status, and drives the shared VS Code launcher. Like the Snowflake and
//! worktrees services it is a cheap, in-memory adapter — no async setup, no
//! secret persisted.
//!
//! Phase 3 additionally starts the engine-owned **transcript watcher**
//! ([`start_watcher`](SessionsService::start_watcher)) so sessions started before
//! the daemon — or working through the hook-silent "thinking window" — are still
//! discovered and marked active. See ADR-0052.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use git2::Repository;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::service::{DaemonService, MenuAction, MenuItem, MenuSnapshot, ServiceStatus};
use crate::daemon::services::worktrees::focus_window;
use crate::sessions::{ObserveRequest, SessionEntry, SessionState, SessionsRegistry, WindowReport};

/// The sessions service name (the control-socket routing key).
pub const SERVICE_NAME: &str = "sessions";

/// The tray submenu title.
const SUBMENU_TITLE: &str = "Claude Sessions";

/// A running background transcript-watcher task and the token that stops it.
struct WatcherTask {
    /// Cancelled by `shutdown` to end the watch loop.
    token: CancellationToken,
    /// The spawned loop, awaited on shutdown so it fully unwinds.
    handle: JoinHandle<()>,
}

/// Hosts the cross-window [`SessionsRegistry`] as a [`DaemonService`].
pub struct SessionsService {
    /// The registry this adapter routes ops to. Behind an `Arc` so the
    /// background transcript-watcher task can feed it off the main thread.
    registry: Arc<SessionsRegistry>,
    /// The background transcript-watcher task, once started (`None` in tests /
    /// with no runtime).
    watcher: Mutex<Option<WatcherTask>>,
}

impl SessionsService {
    /// Creates the service with an empty registry. Cheap — no I/O and no task;
    /// the daemon calls [`start_watcher`](Self::start_watcher) to begin the
    /// transcript watcher, while tests use the bare service.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Arc::new(SessionsRegistry::new()),
            watcher: Mutex::new(None),
        }
    }

    /// Starts the engine-owned transcript watcher (Feed 2): a background task
    /// that scans `~/.claude/projects/**/*.jsonl` for new/growing transcripts and
    /// feeds the registry, so a session started before the daemon — or working
    /// through the hook-silent thinking window — is still discovered and marked
    /// active. Idempotent, and a no-op outside a tokio runtime (mirroring the
    /// worktrees menu-refresh and Snowflake keep-alive tasks), so unit tests that
    /// build a bare service start no watcher.
    pub fn start_watcher(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!("no tokio runtime; sessions transcript watcher not started");
            return;
        }
        let mut guard = self.watcher.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_some() {
            return;
        }
        let token = CancellationToken::new();
        let handle = crate::sessions::watcher::spawn(self.registry.clone(), token.clone());
        *guard = Some(WatcherTask { token, handle });
    }

    /// The registry, for tests driving the service directly.
    #[cfg(test)]
    pub(crate) fn registry(&self) -> &Arc<SessionsRegistry> {
        &self.registry
    }
}

impl Default for SessionsService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DaemonService for SessionsService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    async fn handle(&self, op: &str, payload: Value) -> Result<Value> {
        match op {
            "observe" => {
                let mut req: ObserveRequest =
                    serde_json::from_value(payload).context("invalid `observe` payload")?;
                if req.session_id.trim().is_empty() {
                    bail!("`observe` requires a non-empty `session_id`");
                }
                // Enrich `repo` from `cwd` on a blocking thread (git2 disk I/O),
                // exactly like the worktrees adapter — the engine stores only what
                // it is handed and never touches the disk under its lock. Skip when
                // a caller already supplied `repo`.
                if req.repo.is_none() {
                    if let Some(cwd) = req.cwd.clone() {
                        req.repo = tokio::task::spawn_blocking(move || repo_name_for(&cwd))
                            .await
                            .unwrap_or_default();
                    }
                }
                self.registry.observe(req);
                Ok(json!({ "ok": true }))
            }
            "end" => {
                let session_id = require_str(&payload, "session_id", "end")?;
                let reason = payload.get("reason").and_then(Value::as_str);
                Ok(json!({ "ended": self.registry.end(session_id, reason) }))
            }
            "window" => {
                let req: WindowReport =
                    serde_json::from_value(payload).context("invalid `window` payload")?;
                if req.key.trim().is_empty() {
                    bail!("`window` requires a non-empty `key`");
                }
                self.registry.report_window(req);
                Ok(json!({ "ok": true }))
            }
            "window-unregister" => {
                let key = require_str(&payload, "key", "window-unregister")?;
                Ok(json!({ "removed": self.registry.unregister_window(key) }))
            }
            "list" => Ok(json!({ "sessions": self.registry.list() })),
            other => bail!("unknown sessions op: {other}"),
        }
    }

    fn menu(&self) -> MenuSnapshot {
        // Pure formatting of stored entries — `repo` was enriched at observe
        // time, so `menu()` does no git I/O and honours the trait's "cheap, must
        // not block" contract without a background cache (unlike worktrees).
        MenuSnapshot {
            title: SUBMENU_TITLE.to_string(),
            items: menu_items_for(&self.registry.list()),
        }
    }

    async fn menu_action(&self, action_id: &str) -> Result<()> {
        if let Some(session_id) = action_id.strip_prefix("focus:") {
            let folder = self.registry.focus_folder(session_id).ok_or_else(|| {
                anyhow!("session {session_id} is not open in a known VS Code window")
            })?;
            focus_window(&folder)?;
            return Ok(());
        }
        bail!("unknown sessions menu action: {action_id}")
    }

    async fn status(&self) -> ServiceStatus {
        let sessions = self.registry.list();
        let summary = status_summary(&sessions);
        ServiceStatus {
            name: SERVICE_NAME.to_string(),
            healthy: true,
            summary,
            detail: json!({ "sessions": sessions }),
        }
    }

    async fn shutdown(&self) {
        // Stop the transcript watcher; the registry itself is in-memory with
        // nothing to drain. Take the task out from under the lock first so the
        // `std::Mutex` is never held across the `.await`.
        let task = self
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.token.cancel();
            let _ = task.handle.await;
        }
    }
}

/// Extracts a required string `field` from an op payload, erroring with the op
/// name when it is absent or not a string.
fn require_str<'a>(payload: &'a Value, field: &str, op: &str) -> Result<&'a str> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("`{op}` requires `{field}`"))
}

/// The repository name for a session's `cwd`, derived from the discovered
/// repo's common dir (so a session inside a linked worktree names its parent
/// repo, matching the worktrees enrichment). Best-effort: `None` when `cwd` is
/// not inside a git repo. Pure disk I/O; called on a blocking thread.
fn repo_name_for(cwd: &Path) -> Option<String> {
    let repo = Repository::discover(cwd).ok()?;
    main_repo_name(repo.commondir())
}

/// The main repository's directory name from git's common dir — the same
/// derivation the worktrees adapter uses: for the `<repo>/.git` layout the
/// working-tree directory's name, for a bare `<name>.git` that name with the
/// suffix stripped.
fn main_repo_name(commondir: &Path) -> Option<String> {
    let file_name = commondir.file_name()?.to_string_lossy().into_owned();
    if file_name == ".git" {
        commondir
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
    } else {
        Some(
            file_name
                .strip_suffix(".git")
                .unwrap_or(&file_name)
                .to_string(),
        )
    }
}

/// A one-line `status` summary: the live session count and a per-state tally.
fn status_summary(sessions: &[SessionEntry]) -> String {
    if sessions.is_empty() {
        return "0 session(s)".to_string();
    }
    let mut working = 0;
    let mut waiting = 0;
    let mut idle = 0;
    for s in sessions {
        match s.state {
            SessionState::Working | SessionState::Starting => working += 1,
            SessionState::WaitingForInput | SessionState::WaitingForPermission => waiting += 1,
            SessionState::Idle | SessionState::Ended => idle += 1,
        }
    }
    format!(
        "{} session(s): {working} working, {waiting} waiting, {idle} idle",
        sessions.len()
    )
}

/// A short glyph marking a session's state in the tray label.
fn state_glyph(state: SessionState) -> &'static str {
    match state {
        SessionState::Starting => "…",
        SessionState::Working => "⚙",
        SessionState::Idle => "◦",
        SessionState::WaitingForInput => "?",
        SessionState::WaitingForPermission => "!",
        SessionState::Ended => "×",
    }
}

/// A short human name for a session: its repo, else its cwd basename, else the
/// truncated session id.
fn display_name(entry: &SessionEntry) -> String {
    if let Some(repo) = &entry.repo {
        return repo.clone();
    }
    if let Some(cwd) = &entry.cwd {
        if let Some(name) = cwd.file_name() {
            return name.to_string_lossy().into_owned();
        }
    }
    // A session with neither repo nor cwd: a short id prefix is still a handle.
    entry.session_id.chars().take(8).collect()
}

/// The tray items for the session set: a placeholder when empty, else one line
/// per session (`<name> <glyph> <state>`). A session embedded in a VS Code window
/// is clickable (its click focuses that window via the `focus:` action); a
/// terminal session is a non-clickable status line, since the daemon has no
/// window to focus.
fn menu_items_for(sessions: &[SessionEntry]) -> Vec<MenuItem> {
    use crate::sessions::Source;
    if sessions.is_empty() {
        return vec![MenuItem::Label("No active sessions".to_string())];
    }
    sessions
        .iter()
        .map(|entry| {
            let label = format!(
                "{} {} {}",
                display_name(entry),
                state_glyph(entry.state),
                state_label(entry.state),
            );
            match &entry.source {
                Source::VsCode { .. } => MenuItem::Action(MenuAction {
                    id: format!("focus:{}", entry.session_id),
                    label,
                    enabled: true,
                }),
                Source::Terminal => MenuItem::Label(label),
            }
        })
        .collect()
}

/// A short lowercase label for a session state, for the tray line and any
/// human-readable rendering.
fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Starting => "starting",
        SessionState::Working => "working",
        SessionState::Idle => "idle",
        SessionState::WaitingForInput => "waiting",
        SessionState::WaitingForPermission => "permission",
        SessionState::Ended => "ended",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sessions::{NotificationKind, SessionEvent, Source};
    use std::path::PathBuf;

    fn service() -> SessionsService {
        SessionsService::new()
    }

    #[tokio::test]
    async fn observe_then_list_round_trips() {
        let svc = service();
        let ok = svc
            .handle(
                "observe",
                json!({ "session_id": "s1", "event": "session_start", "cwd": "/tmp/x" }),
            )
            .await
            .unwrap();
        assert_eq!(ok, json!({ "ok": true }));

        let listed = svc.handle("list", Value::Null).await.unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], "s1");
        assert_eq!(sessions[0]["state"], "starting");
    }

    #[tokio::test]
    async fn observe_rejects_blank_session_id() {
        let svc = service();
        let err = svc
            .handle("observe", json!({ "session_id": "  ", "event": "stop" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("session_id"), "{err}");
    }

    #[tokio::test]
    async fn end_marks_ended() {
        let svc = service();
        svc.handle(
            "observe",
            json!({ "session_id": "s1", "event": "pre_tool_use" }),
        )
        .await
        .unwrap();
        let reply = svc
            .handle("end", json!({ "session_id": "s1", "reason": "clear" }))
            .await
            .unwrap();
        assert_eq!(reply, json!({ "ended": true }));
        // Ending an unknown session reports false, not an error.
        let reply = svc
            .handle("end", json!({ "session_id": "ghost" }))
            .await
            .unwrap();
        assert_eq!(reply, json!({ "ended": false }));
    }

    #[tokio::test]
    async fn window_report_tags_source_and_unregister_removes() {
        let svc = service();
        svc.handle(
            "observe",
            json!({ "session_id": "s1", "event": "pre_tool_use", "cwd": "/home/me/proj/sub" }),
        )
        .await
        .unwrap();
        svc.handle(
            "window",
            json!({ "key": "w1", "folders": ["/home/me/proj"], "tabs": 1, "terminals": 0 }),
        )
        .await
        .unwrap();
        let listed = svc.handle("list", Value::Null).await.unwrap();
        assert_eq!(listed["sessions"][0]["source"]["kind"], "vs_code");
        assert_eq!(listed["sessions"][0]["source"]["window_key"], "w1");

        let removed = svc
            .handle("window-unregister", json!({ "key": "w1" }))
            .await
            .unwrap();
        assert_eq!(removed, json!({ "removed": true }));
        let listed = svc.handle("list", Value::Null).await.unwrap();
        assert_eq!(listed["sessions"][0]["source"]["kind"], "terminal");
    }

    #[tokio::test]
    async fn unknown_op_errors() {
        let svc = service();
        let err = svc.handle("frobnicate", Value::Null).await.unwrap_err();
        assert!(err.to_string().contains("unknown sessions op"), "{err}");
    }

    #[tokio::test]
    async fn status_summarizes_states() {
        let svc = service();
        svc.registry().observe(ObserveRequest {
            session_id: "w".to_string(),
            cwd: None,
            transcript_path: None,
            event: SessionEvent::PreToolUse,
            repo: None,
            model: None,
        });
        svc.registry().observe(ObserveRequest {
            session_id: "p".to_string(),
            cwd: None,
            transcript_path: None,
            event: SessionEvent::Notification(NotificationKind::PermissionPrompt),
            repo: None,
            model: None,
        });
        let status = svc.status().await;
        assert!(status.healthy);
        assert!(
            status.summary.contains("2 session(s)"),
            "{}",
            status.summary
        );
        assert!(status.summary.contains("1 working"), "{}", status.summary);
        assert!(status.summary.contains("1 waiting"), "{}", status.summary);
    }

    #[test]
    fn menu_items_placeholder_when_empty() {
        let items = menu_items_for(&[]);
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], MenuItem::Label(l) if l.contains("No active")));
    }

    #[test]
    fn menu_item_is_clickable_only_for_vscode_sessions() {
        let now = chrono::Utc::now();
        let base = |source: Source| SessionEntry {
            session_id: "sid-12345678".to_string(),
            cwd: Some(PathBuf::from("/p")),
            transcript_path: None,
            repo: Some("proj".to_string()),
            model: None,
            state: SessionState::Working,
            source,
            last_event: SessionEvent::PreToolUse,
            started_at: now,
            last_seen: now,
        };
        // A terminal session is a non-clickable label.
        let terminal = menu_items_for(&[base(Source::Terminal)]);
        assert!(
            matches!(&terminal[0], MenuItem::Label(l) if l.contains("proj") && l.contains("working"))
        );
        // A VS Code session is a clickable focus action.
        let vscode = menu_items_for(&[base(Source::VsCode {
            window_key: "w1".to_string(),
        })]);
        match &vscode[0] {
            MenuItem::Action(a) => assert_eq!(a.id, "focus:sid-12345678"),
            other => panic!("expected an action, got {other:?}"),
        }
    }

    #[test]
    fn repo_name_for_non_repo_is_none() {
        // A path that is not inside any git repo enriches to no repo.
        assert_eq!(repo_name_for(Path::new("/nonexistent/xyz")), None);
    }

    #[test]
    fn main_repo_name_handles_layouts() {
        assert_eq!(
            main_repo_name(Path::new("/home/me/proj/.git")).as_deref(),
            Some("proj")
        );
        assert_eq!(
            main_repo_name(Path::new("/home/me/bare.git")).as_deref(),
            Some("bare")
        );
    }

    /// Builds a session entry for the pure menu/label tests.
    fn entry(id: &str, state: SessionState, repo: Option<&str>, cwd: Option<&str>) -> SessionEntry {
        let now = chrono::Utc::now();
        SessionEntry {
            session_id: id.to_string(),
            cwd: cwd.map(PathBuf::from),
            transcript_path: None,
            repo: repo.map(str::to_string),
            model: None,
            state,
            source: Source::Terminal,
            last_event: SessionEvent::PreToolUse,
            started_at: now,
            last_seen: now,
        }
    }

    #[test]
    fn default_constructs_an_empty_service() {
        let svc = SessionsService::default();
        assert!(svc.registry().list().is_empty());
    }

    #[test]
    fn start_watcher_is_a_noop_outside_a_runtime() {
        // No tokio runtime → the watcher is not started, and shutdown is a no-op.
        let svc = SessionsService::new();
        svc.start_watcher();
        assert!(svc
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none());
    }

    #[tokio::test]
    async fn start_watcher_is_idempotent_and_shutdown_stops_it() {
        let svc = SessionsService::new();
        svc.start_watcher();
        // A second call is a no-op (the guard is already set), not a second task.
        svc.start_watcher();
        assert!(svc
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some());
        // Shutdown cancels and joins the task, clearing the slot.
        svc.shutdown().await;
        assert!(svc
            .watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none());
    }

    #[test]
    fn menu_renders_every_state_and_name_fallback() {
        let sessions = vec![
            entry("s1", SessionState::Starting, Some("repo-a"), Some("/a")),
            entry("s2", SessionState::Working, None, Some("/home/me/proj")),
            entry("s3", SessionState::Idle, None, None),
            entry("s4", SessionState::WaitingForInput, Some("r"), Some("/b")),
            entry(
                "s5",
                SessionState::WaitingForPermission,
                Some("r"),
                Some("/c"),
            ),
            entry("s6", SessionState::Ended, Some("r"), Some("/d")),
        ];
        let items = menu_items_for(&sessions);
        assert_eq!(items.len(), 6);
        // Every line is a non-clickable label (all terminal sessions) and carries
        // the state label + a name (repo, else cwd basename, else id prefix).
        let labels: Vec<&str> = items
            .iter()
            .map(|i| match i {
                MenuItem::Label(l) => l.as_str(),
                _ => panic!("terminal sessions render as labels"),
            })
            .collect();
        assert!(labels[0].contains("repo-a") && labels[0].contains("starting"));
        assert!(labels[1].contains("proj") && labels[1].contains("working")); // cwd basename
        assert!(labels[2].contains("s3") && labels[2].contains("idle")); // id-prefix fallback
        assert!(labels[3].contains("waiting"));
        assert!(labels[4].contains("permission"));
        assert!(labels[5].contains("ended"));
    }

    #[test]
    fn menu_serves_the_snapshot_title() {
        let svc = SessionsService::new();
        svc.registry()
            .observe(observe_req("s1", SessionEvent::Stop, None));
        let snapshot = svc.menu();
        assert_eq!(snapshot.title, SUBMENU_TITLE);
        assert_eq!(snapshot.items.len(), 1);
    }

    /// A small `observe` builder for the adapter tests.
    fn observe_req(id: &str, event: SessionEvent, cwd: Option<&str>) -> ObserveRequest {
        ObserveRequest {
            session_id: id.to_string(),
            cwd: cwd.map(PathBuf::from),
            transcript_path: None,
            event,
            repo: None,
            model: None,
        }
    }

    #[tokio::test]
    async fn menu_action_errors_on_unknown_and_missing_window() {
        let svc = SessionsService::new();
        // An unrecognised action id.
        let err = svc.menu_action("frobnicate").await.unwrap_err();
        assert!(
            err.to_string().contains("unknown sessions menu action"),
            "{err}"
        );
        // A focus of a session that resolves to no VS Code window folder.
        let err = svc.menu_action("focus:nope").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("not open in a known VS Code window"),
            "{err}"
        );
    }

    #[test]
    fn repo_name_for_resolves_a_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_dir).unwrap();
        git2::Repository::init(&repo_dir).unwrap();
        // A path inside the repo enriches to the repo's directory name.
        assert_eq!(repo_name_for(&repo_dir).as_deref(), Some("myrepo"));
    }

    #[tokio::test]
    async fn status_summary_counts_idle_and_ended() {
        let svc = SessionsService::new();
        svc.registry()
            .observe(observe_req("i", SessionEvent::Stop, None)); // idle
        svc.registry().end("i2", None); // unknown → no-op
        svc.registry()
            .observe(observe_req("i2", SessionEvent::PreToolUse, None));
        svc.registry().end("i2", Some("done")); // ended
        let status = svc.status().await;
        // Idle + ended both count toward the "idle" tally.
        assert!(status.summary.contains("2 idle"), "{}", status.summary);
    }
}
