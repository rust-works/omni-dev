//! `ViewModelHub`: the actor that merges the two live daemon feeds
//! (`worktrees`, `sessions`) with local state (row colours, open tabs, the
//! lazy ahead/behind cache) into one published [`WorktreesViewModel`] — the
//! single interface boundary between the daemon-facing data layer and the
//! rendering layer (issue #1585's plan, "Interface boundary" section).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::ahead_behind::{AheadBehindCache, Inputs};
use super::client::WorktreesClient;
use super::local_state::OpenTabs;
use super::row_colors::{RowColorKey, RowColorStore};
use super::supervisor::{self, FeedFrame};
use super::view_model::{self, FeedStatus, WorktreesViewModel};
use super::wire::{SessionsListWire, TreeSnapshotWire};
use crate::daemon::protocol::DaemonEnvelope;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

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
        let rows: Option<Vec<(PathBuf, Inputs)>> = {
            let guard = self.tree_rx.borrow_and_update();
            match &*guard {
                FeedFrame::Live(snapshot) => Some(
                    snapshot
                        .repos
                        .iter()
                        .flat_map(|repo| {
                            repo.worktrees.iter().map(|wt| {
                                let inputs = Inputs {
                                    branch: wt.branch.clone(),
                                    head_sha: wt.head_sha.clone(),
                                    upstream_sha: wt.upstream_sha.clone(),
                                    main_sha: repo.main_sha.clone(),
                                };
                                (PathBuf::from(&wt.path), inputs)
                            })
                        })
                        .collect(),
                ),
                _ => None,
            }
        };
        let Some(rows) = rows else { return };

        let all_paths: Vec<PathBuf> = rows.iter().map(|(path, _)| path.clone()).collect();
        // A worktree whose branch or head/upstream/default-branch commit moved since
        // its ahead/behind was fetched (a commit, a push, a fetch of the default
        // branch) has its cached entry dropped, so the `set_visible` below
        // re-queues a fresh fetch instead of leaving stale counts on screen.
        self.ahead_behind.observe(rows);
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
    use serde_json::json;

    use super::super::view_model::AheadBehindState;
    use super::super::wire::{TreeRepoWire, TreeWorktreeWire};
    use super::*;
    use crate::daemon::testutil::fake_daemon_replies;

    /// A hub whose ahead/behind fetches go to `socket`.
    fn test_hub_on(
        socket: impl Into<PathBuf>,
    ) -> (Hub, watch::Sender<FeedFrame<TreeSnapshotWire>>) {
        let (tree_tx, tree_rx) = watch::channel(FeedFrame::Connecting);
        let (_sessions_tx, sessions_rx) = watch::channel(FeedFrame::Connecting);
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = watch::channel(Arc::new(WorktreesViewModel::default()));
        let hub = Hub {
            tree_rx,
            sessions_rx,
            ahead_behind: AheadBehindCache::new(WorktreesClient::new(socket)),
            row_colors: RowColorStore::default(),
            open_tabs: OpenTabs::default(),
            cmd_rx,
            out_tx,
            generation: 0,
            visible_override: None,
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

    /// Lets the one in-flight fetch settle, so the cache holds an entry (or
    /// nothing, if the fetch failed) rather than a pending marker.
    async fn settle(hub: &mut Hub) {
        tokio::time::timeout(Duration::from_secs(5), hub.ahead_behind.changed())
            .await
            .expect("the in-flight fetch should land promptly");
    }

    /// A daemon reply carrying `ahead`/`behind` for `/repo/wt`.
    fn counts_reply(ahead: usize, behind: usize) -> Value {
        json!({ "ok": true, "payload": { "results": {
            "/repo/wt": { "ahead": ahead, "behind": behind }
        }}})
    }

    /// What `counts_reply(ahead, behind)` becomes in the cache.
    fn known(ahead: usize, behind: usize) -> AheadBehindState {
        AheadBehindState::Known {
            ahead,
            behind,
            main_behind: None,
        }
    }

    #[tokio::test]
    async fn on_tree_changed_refetches_when_head_sha_moves() {
        let (_dir, sock, _server) =
            fake_daemon_replies(vec![counts_reply(1, 0), counts_reply(2, 0)]);
        let (mut hub, tree_tx) = test_hub_on(sock);
        let path = PathBuf::from("/repo/wt");
        let send = |head: &str| {
            tree_tx
                .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some(head)))))
                .unwrap();
        };

        send("aaa");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), known(1, 0));

        // A commit landed: the entry is dropped and the path asked again.
        send("bbb");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), known(2, 0));
    }

    #[tokio::test]
    async fn on_tree_changed_refetches_when_only_the_default_branch_tip_moves() {
        // A `git fetch` that advances only `origin/main` moves no worktree's own refs,
        // but it changes `main_behind` — so the repo's `main_sha` has to be part of
        // what invalidates a cached entry (#2120). Without it the row kept the count
        // it was fetched with until a commit or a push happened to move another OID.
        let (_dir, sock, _server) =
            fake_daemon_replies(vec![counts_reply(1, 0), counts_reply(1, 0)]);
        let (mut hub, tree_tx) = test_hub_on(sock);
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
        assert_eq!(hub.ahead_behind.get(&path), known(1, 0));

        // The same snapshot again drops nothing: an unchanged refresh stays free.
        send("m1");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), known(1, 0));

        // Only the default branch's tip moved: the entry is dropped and re-asked.
        send("m2");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
    }

    #[tokio::test]
    async fn on_tree_changed_refetches_when_only_the_branch_changes() {
        // A branch switch can land on the same commit, so nothing a sha says moves.
        let (_dir, sock, _server) =
            fake_daemon_replies(vec![counts_reply(1, 0), counts_reply(3, 0)]);
        let (mut hub, tree_tx) = test_hub_on(sock);
        let path = PathBuf::from("/repo/wt");
        let send = |branch: &str| {
            let mut wt = worktree("/repo/wt", Some("aaa"));
            wt.branch = Some(branch.to_string());
            tree_tx.send(FeedFrame::Live(snapshot(wt))).unwrap();
        };

        send("one");
        hub.on_tree_changed();
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), known(1, 0));

        send("two");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
    }

    #[tokio::test]
    async fn on_tree_changed_does_not_cache_a_result_for_a_head_that_has_since_moved() {
        // #2145, end to end: a fetch is in flight for HEAD `aaa`, a commit moves the
        // worktree to `bbb`, and the reply computed for `aaa` lands. It must not be
        // cached as current, and the worktree must be asked again without waiting
        // for another tree frame.
        let (_dir, sock, _server) =
            fake_daemon_replies(vec![counts_reply(1, 0), counts_reply(2, 0)]);
        let (mut hub, tree_tx) = test_hub_on(sock);
        let path = PathBuf::from("/repo/wt");
        let send = |head: &str| {
            tree_tx
                .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some(head)))))
                .unwrap();
        };

        send("aaa");
        hub.on_tree_changed();
        send("bbb");
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);

        settle(&mut hub).await;
        // The reply for `aaa` was discarded; the re-ask for `bbb` is in flight.
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), known(2, 0));
    }

    #[tokio::test]
    async fn on_tree_changed_does_not_re_ask_an_expected_but_omitted_row_on_every_frame() {
        // #2143: the snapshot says this branch has an upstream, so a reply that omits
        // its row is a failed computation, not an answer — blank, but not settled. And
        // it must not be asked again by every identical frame that follows: a worktree
        // whose computation fails persistently would cost one daemon call per frame.
        let (_dir, sock, _server) = fake_daemon_replies(vec![json!({
            "ok": true, "payload": { "results": {} }
        })]);
        let (mut hub, tree_tx) = test_hub_on(sock);
        let path = PathBuf::from("/repo/wt");
        let send = |head: &str| {
            let mut wt = worktree("/repo/wt", Some(head));
            wt.branch = Some("main".to_string());
            wt.upstream_sha = Some("bbb".to_string());
            tree_tx.send(FeedFrame::Live(snapshot(wt))).unwrap();
        };

        send("aaa");
        hub.on_tree_changed();
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unknown);

        for _ in 0..3 {
            send("aaa");
            hub.on_tree_changed();
            assert!(!hub.ahead_behind.is_pending(&path), "re-asked on a frame");
            assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unknown);
        }

        // A commit is a new situation, not a retry: it is asked at once.
        send("ccc");
        hub.on_tree_changed();
        assert!(hub.ahead_behind.is_pending(&path));
    }

    #[tokio::test]
    async fn on_tree_changed_settles_an_omitted_row_when_the_snapshot_expected_nothing() {
        // The #2134 criterion, kept: a branch with no upstream in a repo with no
        // default branch has nothing to compute, so an omitted row is the answer and
        // is not asked about again on later frames.
        let (_dir, sock, _server) = fake_daemon_replies(vec![json!({
            "ok": true, "payload": { "results": {} }
        })]);
        let (mut hub, tree_tx) = test_hub_on(sock);
        let path = PathBuf::from("/repo/wt");
        let send = || {
            let mut wt = worktree("/repo/wt", Some("aaa"));
            wt.branch = Some("topic".to_string());
            tree_tx.send(FeedFrame::Live(snapshot(wt))).unwrap();
        };

        send();
        hub.on_tree_changed();
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unavailable);

        send();
        hub.on_tree_changed();
        assert!(!hub.ahead_behind.is_pending(&path));
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unavailable);
    }

    #[tokio::test]
    async fn on_tree_changed_re_asks_a_failed_fetch_on_the_next_frame_without_a_ref_moving() {
        // A fetch that failed is not an answer (#2134): the row has to be asked
        // again by the tree feed's next frame, with every OID unchanged, rather
        // than staying blank until a commit, a push or a fetch happens to move one.
        let (_dir, sock, _server) = fake_daemon_replies(vec![
            json!({ "ok": false, "error": "busy" }),
            counts_reply(2, 1),
        ]);
        let (mut hub, tree_tx) = test_hub_on(sock);
        let path = PathBuf::from("/repo/wt");
        let send = || {
            tree_tx
                .send(FeedFrame::Live(snapshot_with_main_sha(
                    worktree("/repo/wt", Some("aaa")),
                    Some("m1"),
                )))
                .unwrap();
        };

        send();
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
        settle(&mut hub).await;
        // Nothing is cached for the failure: not a count, and not `Unavailable`.
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unknown);

        // The daemon is back and the snapshot is byte-identical.
        send();
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), known(2, 1));
    }

    #[tokio::test]
    async fn on_tree_changed_asks_afresh_about_a_worktree_that_left_and_came_back() {
        let (_dir, sock, _server) =
            fake_daemon_replies(vec![counts_reply(1, 0), counts_reply(1, 0)]);
        let (mut hub, tree_tx) = test_hub_on(sock);
        let path = PathBuf::from("/repo/wt");

        tree_tx
            .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some("aaa")))))
            .unwrap();
        hub.on_tree_changed();
        settle(&mut hub).await;
        assert_eq!(hub.ahead_behind.get(&path), known(1, 0));

        tree_tx
            .send(FeedFrame::Live(snapshot(worktree(
                "/repo/other-wt",
                Some("ccc"),
            ))))
            .unwrap();
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Unknown);

        tree_tx
            .send(FeedFrame::Live(snapshot(worktree("/repo/wt", Some("aaa")))))
            .unwrap();
        hub.on_tree_changed();
        assert_eq!(hub.ahead_behind.get(&path), AheadBehindState::Loading);
    }
}
