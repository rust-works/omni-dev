//! `ViewModelHub`: the actor that merges the two live daemon feeds
//! (`worktrees`, `sessions`) with local state (row colours, open tabs, the
//! lazy ahead/behind cache) into one published [`WorktreesViewModel`] — the
//! single interface boundary between the daemon-facing data layer and the
//! rendering layer (issue #1585's plan, "Interface boundary" section).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::ahead_behind::AheadBehindCache;
use super::client::WorktreesClient;
use super::local_state::OpenTabs;
use super::row_colors::{RowColorKey, RowColorStore};
use super::supervisor::{self, FeedFrame};
use super::view_model::{self, FeedStatus, WorktreesViewModel};
use super::wire::{SessionsListWire, TreeSnapshotWire};
use crate::daemon::protocol::DaemonEnvelope;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// One worktree's identity for the OID-staleness check in
/// `Hub::on_tree_changed` — just enough of the wire row to detect a commit or
/// a push without holding the whole `TreeWorktreeWire`.
struct WorktreeOids {
    path: PathBuf,
    head_sha: Option<String>,
    upstream_sha: Option<String>,
}

/// Commands the rendering layer sends into the hub.
///
/// Phase 1 has no scrollable/collapsible tree pane yet, so the hub treats
/// every worktree in the latest snapshot as visible by default; later phases
/// narrow that with `SetVisibleRows` once there is a real notion of "on
/// screen". The row-colour and open-tab commands are unused until the action
/// layer (Phase 2) and the embedded-terminal tab lifecycle (Phase 3) land,
/// but the command surface is defined here now since it *is* the hub/render
/// interface boundary — `apply` below already handles every variant.
#[derive(Debug, Clone)]
#[allow(dead_code)] // constructed by the render layer starting Phase 2/3
pub enum HubCommand {
    SetOpenTab(PathBuf),
    ClearOpenTab(PathBuf),
    SetRowColor(RowColorKey, String),
    ClearRowColor(RowColorKey),
    ClearAllRowColors,
    SetVisibleRows(Vec<PathBuf>),
}

/// The handle the rendering layer holds: `view` to redraw from, `commands` to
/// report state changes into the hub.
pub struct ViewModelHandle {
    pub view: watch::Receiver<Arc<WorktreesViewModel>>,
    /// Unused by Phase 1's read-only render loop; wired up starting Phase 2
    /// (actions, row-colour edits) and Phase 3 (tab lifecycle).
    #[allow(dead_code)]
    pub commands: mpsc::UnboundedSender<HubCommand>,
}

/// Spawns the hub actor and returns the handle the rendering layer drives it
/// with. `socket` is the resolved daemon control-socket path.
pub fn spawn(socket: PathBuf, cancel: CancellationToken) -> ViewModelHandle {
    let (tree_rx, _tree_task) = supervisor::spawn_subscription::<TreeSnapshotWire>(
        socket.clone(),
        DaemonEnvelope::service("worktrees", "subscribe", Value::Null),
        DaemonEnvelope::service("worktrees", "tree", Value::Null),
        POLL_INTERVAL,
        cancel.clone(),
    );
    let (sessions_rx, _sessions_task) = supervisor::spawn_subscription::<SessionsListWire>(
        socket.clone(),
        DaemonEnvelope::service("sessions", "subscribe", Value::Null),
        DaemonEnvelope::service("sessions", "list", Value::Null),
        POLL_INTERVAL,
        cancel.clone(),
    );
    // Best-effort: a corrupt/unreadable row-colours file degrades to "no
    // colours" rather than blocking startup.
    let row_colors = RowColorStore::load(None).unwrap_or_default();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = watch::channel(Arc::new(WorktreesViewModel::default()));

    let hub = Hub {
        tree_rx,
        sessions_rx,
        ahead_behind: AheadBehindCache::new(WorktreesClient::new(socket)),
        row_colors,
        open_tabs: OpenTabs::default(),
        cmd_rx,
        out_tx,
        generation: 0,
        visible_override: None,
        last_seen_oids: HashMap::new(),
        cancel,
    };
    tokio::spawn(hub.run());

    ViewModelHandle {
        view: out_rx,
        commands: cmd_tx,
    }
}

struct Hub {
    tree_rx: watch::Receiver<FeedFrame<TreeSnapshotWire>>,
    sessions_rx: watch::Receiver<FeedFrame<SessionsListWire>>,
    ahead_behind: AheadBehindCache,
    row_colors: RowColorStore,
    open_tabs: OpenTabs,
    cmd_rx: mpsc::UnboundedReceiver<HubCommand>,
    out_tx: watch::Sender<Arc<WorktreesViewModel>>,
    generation: u64,
    /// Explicit override from `SetVisibleRows`; `None` means "everything in
    /// the latest tree snapshot" (Phase 1's default, see [`HubCommand`]).
    visible_override: Option<Vec<PathBuf>>,
    /// The `(head_sha, upstream_sha)` each path's cached ahead/behind entry
    /// was last computed against, so a commit or a push (which moves one of
    /// these OIDs — see `TreeWorktreeWire`'s doc comment) invalidates the
    /// stale cache entry instead of leaving it to show counts for a HEAD the
    /// worktree has since moved past.
    last_seen_oids: HashMap<PathBuf, (Option<String>, Option<String>)>,
    cancel: CancellationToken,
}

impl Hub {
    async fn run(mut self) {
        self.publish();
        loop {
            tokio::select! {
                changed = self.tree_rx.changed() => {
                    if changed.is_err() {
                        return; // the supervisor task is gone
                    }
                    self.on_tree_changed();
                }
                changed = self.sessions_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                Some(cmd) = self.cmd_rx.recv() => self.apply(cmd),
                () = self.ahead_behind.changed() => {}
                () = self.cancel.cancelled() => return,
            }
            self.publish();
        }
    }

    fn on_tree_changed(&mut self) {
        let rows: Option<Vec<WorktreeOids>> = {
            let guard = self.tree_rx.borrow_and_update();
            match &*guard {
                FeedFrame::Live(snapshot) => Some(
                    snapshot
                        .repos
                        .iter()
                        .flat_map(|repo| repo.worktrees.iter())
                        .map(|wt| WorktreeOids {
                            path: PathBuf::from(&wt.path),
                            head_sha: wt.head_sha.clone(),
                            upstream_sha: wt.upstream_sha.clone(),
                        })
                        .collect(),
                ),
                _ => None,
            }
        };
        let Some(rows) = rows else { return };

        // A worktree whose head/upstream OID moved since we last fetched its
        // ahead/behind (a commit or a push) invalidates that cache entry, so
        // the next `set_visible` below re-queues a fresh fetch instead of
        // leaving stale counts on screen.
        for row in &rows {
            let oids = (row.head_sha.clone(), row.upstream_sha.clone());
            if self.last_seen_oids.get(&row.path) != Some(&oids) {
                self.ahead_behind.invalidate(&row.path);
                self.last_seen_oids.insert(row.path.clone(), oids);
            }
        }
        self.last_seen_oids
            .retain(|path, _| rows.iter().any(|row| &row.path == path));

        let all_paths: Vec<PathBuf> = rows.into_iter().map(|row| row.path).collect();
        let visible = self.visible_override.clone().unwrap_or(all_paths);
        self.ahead_behind.set_visible(&visible);
    }

    fn apply(&mut self, cmd: HubCommand) {
        match cmd {
            HubCommand::SetOpenTab(path) => self.open_tabs.set(path),
            HubCommand::ClearOpenTab(path) => self.open_tabs.clear(&path),
            HubCommand::SetRowColor(key, color) => {
                if let Err(e) = self.row_colors.set(key, color) {
                    tracing::warn!("worktrees ui: failed to set row colour: {e:#}");
                }
            }
            HubCommand::ClearRowColor(key) => {
                if let Err(e) = self.row_colors.clear(&key) {
                    tracing::warn!("worktrees ui: failed to clear row colour: {e:#}");
                }
            }
            HubCommand::ClearAllRowColors => {
                if let Err(e) = self.row_colors.clear_all() {
                    tracing::warn!("worktrees ui: failed to clear row colours: {e:#}");
                }
            }
            HubCommand::SetVisibleRows(paths) => {
                self.ahead_behind.set_visible(&paths);
                self.visible_override = Some(paths);
            }
        }
    }

    fn publish(&mut self) {
        self.generation += 1;
        let (tree, worktrees_status) = {
            let guard = self.tree_rx.borrow();
            let status = feed_status(&guard);
            let tree = match &*guard {
                FeedFrame::Live(snapshot) => Some(snapshot.clone()),
                _ => None,
            };
            (tree, status)
        };
        let (sessions, sessions_status) = {
            let guard = self.sessions_rx.borrow();
            let status = feed_status(&guard);
            let sessions = match &*guard {
                FeedFrame::Live(list) => list.sessions.clone(),
                _ => Vec::new(),
            };
            (sessions, status)
        };
        let view = view_model::merge(
            tree.as_ref(),
            &sessions,
            &self.ahead_behind,
            &self.row_colors,
            &self.open_tabs,
            worktrees_status,
            sessions_status,
            self.generation,
        );
        let _ = self.out_tx.send(Arc::new(view));
    }
}

fn feed_status<T>(frame: &FeedFrame<T>) -> FeedStatus {
    match frame {
        FeedFrame::Connecting => FeedStatus::Connecting,
        FeedFrame::Live(_) => FeedStatus::Live,
        FeedFrame::Reconnecting { attempt, retry_in } => FeedStatus::Reconnecting {
            attempt: *attempt,
            retry_in: *retry_in,
        },
        FeedFrame::Polling => FeedStatus::Polling,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::wire::{TreeRepoWire, TreeWorktreeWire};
    use super::*;

    fn test_hub() -> (Hub, watch::Sender<FeedFrame<TreeSnapshotWire>>) {
        let (tree_tx, tree_rx) = watch::channel(FeedFrame::Connecting);
        let (_sessions_tx, sessions_rx) = watch::channel(FeedFrame::Connecting);
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = watch::channel(Arc::new(WorktreesViewModel::default()));
        let hub = Hub {
            tree_rx,
            sessions_rx,
            ahead_behind: AheadBehindCache::new(WorktreesClient::new(
                "/tmp/nonexistent-omni-dev-hub-test.sock",
            )),
            row_colors: RowColorStore::default(),
            open_tabs: OpenTabs::default(),
            cmd_rx,
            out_tx,
            generation: 0,
            visible_override: None,
            last_seen_oids: HashMap::new(),
            cancel: CancellationToken::new(),
        };
        (hub, tree_tx)
    }

    fn worktree(path: &str, head_sha: Option<&str>) -> TreeWorktreeWire {
        TreeWorktreeWire {
            path: path.to_string(),
            branch: None,
            head_sha: head_sha.map(str::to_string),
            upstream_sha: None,
            is_main: false,
            open: false,
            window_key: None,
            pr: None,
            pr_none: false,
            operation: None,
            rebasing: false,
            pushing: false,
        }
    }

    fn snapshot(wt: TreeWorktreeWire) -> TreeSnapshotWire {
        TreeSnapshotWire {
            repos: vec![TreeRepoWire {
                main_repo: "repo".to_string(),
                github: None,
                root: "/repo".to_string(),
                polling_enabled: false,
                worktrees: vec![wt],
            }],
            show_closed: false,
        }
    }

    // `on_tree_changed` calls `AheadBehindCache::set_visible`, which spawns a
    // fetch task via `tokio::spawn` for a newly-seen path, so these need a
    // runtime context (`#[tokio::test]`) even though nothing here is awaited.

    #[tokio::test]
    async fn on_tree_changed_records_each_paths_current_oids() {
        let (mut hub, tree_tx) = test_hub();
        tree_tx
            .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some("aaa")))))
            .unwrap();
        hub.on_tree_changed();
        assert_eq!(
            hub.last_seen_oids.get(&PathBuf::from("/repo/wt")),
            Some(&(Some("aaa".to_string()), None))
        );
    }

    #[tokio::test]
    async fn on_tree_changed_invalidates_the_ahead_behind_cache_when_head_sha_moves() {
        let (mut hub, tree_tx) = test_hub();
        let path = PathBuf::from("/repo/wt");
        tree_tx
            .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some("aaa")))))
            .unwrap();
        hub.on_tree_changed();
        // Fetch is now in flight (or unreachable-socket-failed) for this
        // path; either way it is no longer Unknown.
        assert_ne!(
            hub.ahead_behind.get(&path),
            super::super::view_model::AheadBehindState::Unknown
        );

        // A new head_sha (a commit landed) must update the tracked OIDs —
        // the actual cache-drop behaviour of `invalidate` is covered by
        // ahead_behind.rs's own tests; this asserts the bookkeeping that
        // decides *when* to call it.
        tree_tx
            .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some("bbb")))))
            .unwrap();
        hub.on_tree_changed();
        assert_eq!(
            hub.last_seen_oids.get(&path),
            Some(&(Some("bbb".to_string()), None))
        );
    }

    #[tokio::test]
    async fn on_tree_changed_forgets_oids_for_worktrees_no_longer_in_the_snapshot() {
        let (mut hub, tree_tx) = test_hub();
        tree_tx
            .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some("aaa")))))
            .unwrap();
        hub.on_tree_changed();
        assert!(hub.last_seen_oids.contains_key(&PathBuf::from("/repo/wt")));

        tree_tx
            .send(FeedFrame::Live(snapshot(worktree(
                "/repo/other-wt",
                Some("ccc"),
            ))))
            .unwrap();
        hub.on_tree_changed();
        assert!(!hub.last_seen_oids.contains_key(&PathBuf::from("/repo/wt")));
        assert!(hub
            .last_seen_oids
            .contains_key(&PathBuf::from("/repo/other-wt")));
    }
}
