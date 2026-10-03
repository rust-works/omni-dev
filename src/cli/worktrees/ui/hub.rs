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

/// The refs one worktree's ahead/behind is computed from, as the snapshot reports
/// them: `(head_sha, upstream_sha, main_sha)`. Equal means an equal answer, so a
/// change in any one is what invalidates a cached entry.
type SeenOids = (Option<String>, Option<String>, Option<String>);

/// One worktree's identity for the OID-staleness check in
/// `Hub::on_tree_changed` — just enough of the wire row to detect a commit, a
/// push or a default-branch fetch without holding the whole `TreeWorktreeWire`.
struct WorktreeOids {
    path: PathBuf,
    head_sha: Option<String>,
    upstream_sha: Option<String>,
    /// The *repo's* default-branch tip, repeated on each of its worktrees.
    main_sha: Option<String>,
}

/// Commands the rendering layer sends into the hub.
///
/// Phase 2's `Dispatcher` (`actions.rs`) constructs `SetRowColor`/
/// `ClearRowColor` for the `c`/`C` keybindings and `ClearAllRowColors` for
/// Phase 5's `alt-⇧c`; `SetOpenTab`/`ClearOpenTab` follow Phase 3's tab
/// lifecycle, and `SetVisibleRows` reports Phase 4c's on-screen rows so
/// ahead/behind is fetched only for those.
#[derive(Debug, Clone)]
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
    /// Constructed and sent by `actions::Dispatcher` (Phase 2).
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
    /// The `(head_sha, upstream_sha, main_sha)` each path's cached ahead/behind
    /// entry was last computed against, so a commit, a push or a fetch of the
    /// default branch (which moves one of these OIDs — see `TreeWorktreeWire`'s
    /// doc comment) invalidates the stale cache entry instead of leaving it to
    /// show counts for a HEAD the worktree has since moved past. `main_sha` is
    /// the one a fetch of only `origin/main` moves, which is the only way
    /// `main_behind` changes with nothing else in the row moving (#2120).
    last_seen_oids: HashMap<PathBuf, SeenOids>,
    cancel: CancellationToken,
}

impl Hub {
    async fn run(mut self) {
        self.publish();
        loop {
            tokio::select! {
                changed = self.tree_rx.changed() => {
                    if changed.is_err() {
                        // The supervisor task's sender was dropped — normal on
                        // a deliberate cancel, but also the only symptom of an
                        // unexpected panic inside it, so this must not go
                        // silent: without a log line here, the UI just freezes
                        // on its last snapshot with no visible cause.
                        tracing::warn!("worktrees ui: tree feed supervisor task ended; hub stopping");
                        return;
                    }
                    self.on_tree_changed();
                }
                changed = self.sessions_rx.changed() => {
                    if changed.is_err() {
                        tracing::warn!("worktrees ui: sessions feed supervisor task ended; hub stopping");
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
                        .flat_map(|repo| {
                            repo.worktrees.iter().map(|wt| WorktreeOids {
                                path: PathBuf::from(&wt.path),
                                head_sha: wt.head_sha.clone(),
                                upstream_sha: wt.upstream_sha.clone(),
                                main_sha: repo.main_sha.clone(),
                            })
                        })
                        .collect(),
                ),
                _ => None,
            }
        };
        let Some(rows) = rows else { return };

        // A worktree whose head/upstream/default-branch OID moved since we last
        // fetched its ahead/behind (a commit, a push or a default-branch fetch)
        // invalidates that cache entry, so the next `set_visible` below re-queues
        // a fresh fetch instead of leaving stale counts on screen.
        for row in &rows {
            let oids: SeenOids = (
                row.head_sha.clone(),
                row.upstream_sha.clone(),
                row.main_sha.clone(),
            );
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
        snapshot_with_main_sha(wt, None)
    }

    fn snapshot_with_main_sha(wt: TreeWorktreeWire, main_sha: Option<&str>) -> TreeSnapshotWire {
        TreeSnapshotWire {
            repos: vec![TreeRepoWire {
                main_repo: "repo".to_string(),
                github: None,
                root: "/repo".to_string(),
                polling_enabled: false,
                main_sha: main_sha.map(str::to_string),
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
            Some(&(Some("aaa".to_string()), None, None))
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
            Some(&(Some("bbb".to_string()), None, None))
        );
    }

    /// Lets the one in-flight fetch (to an unreachable socket) settle, so the
    /// cache holds an entry rather than a pending marker.
    async fn settle(hub: &mut Hub) {
        tokio::time::timeout(Duration::from_secs(5), hub.ahead_behind.changed())
            .await
            .expect("the fetch to an unreachable socket should fail promptly");
    }

    #[tokio::test]
    async fn on_tree_changed_refetches_when_only_the_default_branch_tip_moves() {
        // A `git fetch` that advances only `origin/main` moves no worktree's own refs,
        // but it changes `main_behind` — so the repo's `main_sha` has to be part of
        // what invalidates a cached entry (#2120). Without it the row kept the count
        // it was fetched with until a commit or a push happened to move another OID.
        use super::super::view_model::AheadBehindState;
        let (mut hub, tree_tx) = test_hub();
        let path = PathBuf::from("/repo/wt");
        let send = |main_sha: &str| {
            tree_tx
                .send(FeedFrame::Live(snapshot_with_main_sha(
                    worktree("/repo/wt", Some("aaa")),
                    Some(main_sha),
                )))
                .unwrap();
        };

        send("m1");
        hub.on_tree_changed();
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unavailable);

        // The same snapshot again drops nothing: an unchanged refresh stays free.
        send("m1");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unavailable);

        // Only the default branch's tip moved: the entry is dropped and re-asked.
        send("m2");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
        assert_eq!(
            hub.last_seen_oids.get(&path),
            Some(&(Some("aaa".to_string()), None, Some("m2".to_string())))
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
