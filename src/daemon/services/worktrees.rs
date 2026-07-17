//! The worktrees daemon service.
//!
//! A thin adapter that hosts the cross-window [`WorktreesRegistry`] under the
//! daemon's lifecycle and exposes register/heartbeat/unregister/list/tree/open
//! over the control socket, plus a tray submenu with a per-window "focus" action.
//! The `open` op (#1266) focuses/opens an arbitrary worktree folder in VS Code
//! through the **same** launcher path the tray uses, so a socket client (the
//! companion's double-click) shares the tested guard and launcher resolution
//! rather than duplicating them.
//!
//! All registry state and liveness logic (the `Mutex<HashMap>`, TTL reaping, the
//! entry cap/eviction) lives in [`crate::worktrees`]; this adapter only routes
//! ops, renders the menu/status, and drives the VS Code launcher. Like the
//! Snowflake service it is a cheap, in-memory adapter — no async setup, no
//! secret persisted.
//!
//! The adapter also computes the **per-worktree git enrichment** (current
//! branch, ahead/behind counts, and the parent repository a linked worktree
//! belongs to) on read via `git2` (#1186), keeping the companion a thin reporter
//! of raw folder paths (ADR-0040). The engine stores only what the companion
//! sends; disk I/O for the enrichment lives here, alongside the launcher, never
//! under the registry lock.
//!
//! The `tree` op (#1265) inverts the data model for the companion's tree view:
//! from the open windows the adapter derives the **distinct repositories**, then
//! enumerates **all** of each repo's worktrees (main working tree +
//! [`Repository::worktrees`]), enriches each (reusing [`git_status`]), tags the
//! GitHub identity of `origin`, and joins the open windows back on by
//! canonicalized path. The open-window registry stays the liveness source;
//! "is a window open on it?" becomes a per-worktree attribute. All of this is
//! git disk I/O, so it runs on a blocking thread, never under the registry lock.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::pr_status::{PrBadge, PrCheckState, PrStatusCache, PrTarget};
use async_trait::async_trait;
use git2::{Repository, RepositoryState, Status, StatusOptions, WorktreeLockStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::service::{
    DaemonService, MenuAction, MenuItem, MenuSnapshot, ServiceStatus, ServiceStream,
};
use crate::worktrees::{RegisterRequest, WindowEntry, WorktreesRegistry};

/// The worktrees service name (the control-socket routing key).
pub const SERVICE_NAME: &str = "worktrees";

/// The tray submenu title.
const SUBMENU_TITLE: &str = "Worktrees";

/// Environment override for the VS Code launcher used by the "focus" tray
/// action, for when the daemon runs under launchd with a minimal `PATH`.
const VSCODE_BIN_ENV: &str = "OMNI_DEV_VSCODE_BIN";

/// Environment override for [`menu_refresh_interval`] (whole seconds; a blank,
/// non-numeric, or `0` value falls back to [`DEFAULT_MENU_REFRESH_INTERVAL`]).
const ENV_MENU_REFRESH_INTERVAL: &str = "OMNI_DEV_DAEMON_MENU_REFRESH";

/// Default cadence at which the background task recomputes the tray menu snapshot
/// off the main thread when `OMNI_DEV_DAEMON_MENU_REFRESH` is unset. The macOS
/// tray polls `menu()` ~1 Hz and always serves this cache, never doing git I/O on
/// the GUI thread (which would peg a core and stall shutdown — the #1186
/// regression); the interval only governs how stale that cached branch/sync state
/// may be when the menu is opened.
///
/// Raised from 2 s to 10 s (#1305): this refresh is an independent per-window git
/// walk that the subscription-stream coalescing (#1303) never touched — it was
/// the dominant idle-CPU cost — so relaxing it cuts that cost ~5× while leaving
/// menu open-latency unchanged (the cache still serves instantly).
const DEFAULT_MENU_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// The resolved tray menu-refresh cadence: `OMNI_DEV_DAEMON_MENU_REFRESH` (whole
/// seconds) when valid, else [`DEFAULT_MENU_REFRESH_INTERVAL`].
fn menu_refresh_interval() -> Duration {
    crate::daemon::server::duration_secs_from_env(
        ENV_MENU_REFRESH_INTERVAL,
        DEFAULT_MENU_REFRESH_INTERVAL,
    )
}

/// Environment override for [`pr_poll_interval`] — the cadence at which the PR
/// badge poller re-asks GitHub **while a badge is still pending** (whole seconds;
/// a blank, non-numeric, or `0` value falls back to [`DEFAULT_PR_POLL_INTERVAL`]).
const ENV_PR_POLL_INTERVAL: &str = "OMNI_DEV_DAEMON_PR_POLL";

/// Default cadence for the PR badge poller while CI is in flight (#1337).
///
/// Matches `gh pr checks --watch`, which uses 10 s when a human is actively
/// watching a run — which is exactly this situation. It is affordable because the
/// poll costs **1 point** regardless of how many repos, worktrees, or windows are
/// open: 10 s sustained is ~360 points/hour against a 5,000/hour budget, and only
/// while something is actually pending.
const DEFAULT_PR_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// The ceiling the poller backs off to once every badge is terminal.
///
/// Nothing is expected to change, so this is a liveness heartbeat, not a watch.
/// The 30-minute figure is the cross-tool consensus for background PR polling
/// (vscode-pull-request-github's backoff ceiling, gh-dash's and GitLens's
/// defaults). The backoff exists for battery and wakeups rather than budget — at
/// 1 point per poll the budget never binds.
const MAX_PR_POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// The resolved PR-poll cadence: `OMNI_DEV_DAEMON_PR_POLL` (whole seconds) when
/// valid, else [`DEFAULT_PR_POLL_INTERVAL`].
fn pr_poll_interval() -> Duration {
    crate::daemon::server::duration_secs_from_env(ENV_PR_POLL_INTERVAL, DEFAULT_PR_POLL_INTERVAL)
}

/// A running background menu-refresh task and the token that stops it.
struct RefreshTask {
    /// Cancelled by `shutdown` to end the refresh loop.
    token: CancellationToken,
    /// The spawned loop, awaited on shutdown so it fully unwinds.
    handle: JoinHandle<()>,
}

/// Whether this tick should spend a `gh` call.
///
/// The poller wakes far more often than it fetches — waking is a cached snapshot
/// read, fetching is a subprocess and a network round trip. Two things justify the
/// call: the watched state **moved** (a window opened, or a commit landed — the
/// latter being invisible to the change-notify, so only looking finds it), or the
/// **backoff elapsed** and it is simply time to look again.
///
/// Pure so the policy is testable directly: from outside, the only evidence of it
/// is *when* a subprocess runs, which a test cannot pin down without either flaking
/// or passing for the wrong reason.
fn pr_should_fetch(moved: bool, since_last_fetch: Option<Duration>, backoff: Duration) -> bool {
    // `map_or(true, ..)` rather than `is_none_or`: the latter is stable only
    // since 1.82 and this crate's MSRV is 1.80.
    moved || since_last_fetch.map_or(true, |elapsed| elapsed >= backoff)
}

/// The next PR-poll delay: `base` while something is still pending, else double
/// the current delay up to [`MAX_PR_POLL_INTERVAL`].
///
/// A pure function rather than three copies inline, because the cadence is the part
/// worth pinning: it is only observable from outside as timing, which is exactly
/// what a test cannot assert without flaking. A failed poll passes `pending: false`,
/// so a persistent failure backs off instead of being retried hard.
fn next_pr_poll_delay(current: Duration, base: Duration, pending: bool) -> Duration {
    if pending {
        base
    } else {
        current.saturating_mul(2).min(MAX_PR_POLL_INTERVAL)
    }
}

/// A running background PR-badge poll task and the token that stops it.
struct PollerTask {
    /// Cancelled by `shutdown` to end the poll loop.
    token: CancellationToken,
    /// The spawned loop, awaited on shutdown so it fully unwinds.
    handle: JoinHandle<()>,
}

/// One thing the PR poller watches: a badge target, the commit the worktree
/// carrying it currently has checked out, and the commit its upstream points at.
///
/// The two OIDs are what make local work observable to the poller. A window
/// opening bumps the registry's change-notify, but nothing notifies the daemon
/// when you commit or push — so the poller compares this against the previous
/// tick's and treats any move as "go and ask now". Both are needed because they
/// move on different actions: committing moves the head, while pushing moves
/// **only** the upstream (#1344). Without the upstream, a push — the very thing
/// that starts the CI run a badge reports — did not re-ask, and the badge sat at
/// `●` until the backoff elapsed, up to its 30-minute ceiling.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PrWatch {
    /// The (repo, branch) to resolve a badge for.
    target: PrTarget,
    /// That worktree's HEAD, or `None` for an unborn one.
    head_sha: Option<String>,
    /// That branch's upstream tip, or `None` when it tracks no upstream.
    upstream_sha: Option<String>,
}

/// Extracts what the poller watches — the badge targets and their local heads and
/// upstream tips — from a `tree` snapshot.
///
/// Reading them back off the snapshot — rather than walking git again — means the
/// poller reuses the coalescing [`TreeSnapshotCache`] build instead of adding a
/// second independent per-worktree git walk, which is the idle-CPU cost #1305 went
/// out of its way to remove. Only GitHub repos with a branch contribute; the result
/// is sorted and deduped so N worktrees of one repo on one branch ask once.
fn pr_watch_from_snapshot(snapshot: &Value) -> Vec<PrWatch> {
    let mut out = Vec::new();
    for repo in snapshot
        .get("repos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(github) = repo.get("github") else {
            continue;
        };
        let (Some(owner), Some(name)) = (
            github.get("owner").and_then(Value::as_str),
            github.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        for wt in repo
            .get("worktrees")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(branch) = wt.get("branch").and_then(Value::as_str) {
                out.push(PrWatch {
                    head_sha: wt
                        .get("head_sha")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    upstream_sha: wt
                        .get("upstream_sha")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    target: PrTarget {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        branch: branch.to_string(),
                    },
                });
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The (repo, branch) pairs to resolve badges for — [`pr_watch_from_snapshot`]
/// without the heads.
#[cfg(test)]
fn pr_targets_from_snapshot(snapshot: &Value) -> Vec<PrTarget> {
    pr_watch_from_snapshot(snapshot)
        .into_iter()
        .map(|w| w.target)
        .collect()
}

/// Hosts the cross-window [`WorktreesRegistry`] as a [`DaemonService`].
pub struct WorktreesService {
    /// The cross-window registry this adapter routes ops to. Behind an `Arc` so
    /// the background menu-refresh task can read it off the main thread.
    registry: Arc<WorktreesRegistry>,
    /// The most recent tray menu snapshot, recomputed off the main thread by
    /// [`start_menu_refresh`](Self::start_menu_refresh). `menu()` serves a clone
    /// of this so it never blocks on git enrichment. `None` until the first
    /// refresh lands — or when no runtime started a task (e.g. unit tests) — in
    /// which case `menu()` falls back to a one-off inline compute.
    menu_cache: Arc<Mutex<Option<Vec<MenuItem>>>>,
    /// The background refresh task, once started (`None` in tests / no runtime).
    refresh: Mutex<Option<RefreshTask>>,
    /// PR badges resolved by the background poller and read by the tree snapshot
    /// build (#1337). Behind an `Arc` so the poll task and the snapshot builder
    /// share the one cache. Empty until the first poll lands — and always empty
    /// when no poller runs (unit tests), in which case the tree simply carries no
    /// `pr` field, exactly as a pre-#1337 daemon did.
    pr_cache: Arc<PrStatusCache>,
    /// The background PR-badge poll task, once started (`None` in tests / no
    /// runtime).
    poller: Mutex<Option<PollerTask>>,
    /// The shared, coalescing tree-snapshot cache every `subscribe` stream reads
    /// through, so N open windows perform **one** `build_tree` per tick instead
    /// of N (#1303). Behind an `Arc` so each stream holds a cheap handle to the
    /// one cache. The one-shot `tree` op deliberately bypasses it and computes
    /// fresh (it is a rare manual refresh, not part of the per-tick fan-out).
    tree_cache: Arc<TreeSnapshotCache>,
    /// Serializes [`remove_worktree`] across concurrent `close` executes (#1359).
    ///
    /// The extension fans a multi-select delete out into one `close` op per
    /// target, so two executes can reach the prune at once. Their heartbeat waits
    /// overlap freely — that is the point — but the prunes themselves should not:
    /// each op enumerates the repo's worktrees ([`worktree_name_for_path`]) and
    /// then prunes an entry out of that same `.git/worktrees`, so concurrent ops
    /// read a directory a sibling is midway through removing from, and `git2`
    /// promises nothing about that. Precautionary rather than a fix for an
    /// observed corruption — the window is narrow enough that it has not been
    /// reproduced — but serializing costs nothing measurable (the prune is a
    /// directory delete; the wait it follows is seconds) and keeps the fan-out
    /// safe at the source rather than relying on every caller to stay sequential.
    ///
    /// A `tokio` mutex rather than a `std` one: it is held across the
    /// `spawn_blocking` join, which is an `.await`.
    prune_lock: tokio::sync::Mutex<()>,
}

impl WorktreesService {
    /// Creates the service with an empty registry. Cheap — no I/O and no task;
    /// the daemon calls [`start_menu_refresh`](Self::start_menu_refresh) to begin
    /// off-thread menu caching, while tests use the bare service (menu computed
    /// inline on demand).
    #[must_use]
    pub fn new() -> Self {
        let registry = Arc::new(WorktreesRegistry::new());
        let pr_cache = Arc::new(PrStatusCache::new());
        Self {
            registry: registry.clone(),
            menu_cache: Arc::new(Mutex::new(None)),
            refresh: Mutex::new(None),
            pr_cache: pr_cache.clone(),
            poller: Mutex::new(None),
            tree_cache: Arc::new(TreeSnapshotCache::new(registry, pr_cache)),
            prune_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Starts the background task that recomputes the tray menu snapshot every
    /// [`menu_refresh_interval`] **off the main thread** — git enrichment is
    /// blocking disk I/O — and stores it in [`menu_cache`](Self::menu_cache), so
    /// the macOS tray's `menu()` serves a cache instead of running git on the GUI
    /// event loop. Idempotent, and a no-op outside a tokio runtime (mirroring the
    /// Snowflake keep-alive heartbeat), so unit tests that build a bare service
    /// keep computing the menu inline.
    pub fn start_menu_refresh(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!("no tokio runtime; worktrees menu refresh not started");
            return;
        }
        let mut guard = self.refresh.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_some() {
            return;
        }
        let token = CancellationToken::new();
        let loop_token = token.clone();
        let registry = self.registry.clone();
        let cache = self.menu_cache.clone();
        // Resolved once at spawn: the interval is process-stable env config, and
        // re-reading it every loop would be wasted work.
        let interval = menu_refresh_interval();
        let handle = tokio::spawn(async move {
            loop {
                // Snapshot the registry (a cheap lock), then build the menu —
                // which opens repos and parses git config — on a blocking thread,
                // never on this async worker or the tray's main thread.
                let entries = registry.list();
                if let Ok(items) =
                    tokio::task::spawn_blocking(move || menu_items_for(&entries)).await
                {
                    *cache.lock().unwrap_or_else(PoisonError::into_inner) = Some(items);
                }
                tokio::select! {
                    () = loop_token.cancelled() => break,
                    () = tokio::time::sleep(interval) => {}
                }
            }
        });
        *guard = Some(RefreshTask { token, handle });
    }

    /// Starts the background task that keeps PR check badges fresh (#1337).
    ///
    /// This is the half of the badge nothing else can do. Badges used to be
    /// resolved extension-side on repo-expand, so they were recomputed only when a
    /// repo node's children were rebuilt — and the streamed snapshot carries
    /// worktree topology, not CI. While CI ran and no window opened or closed,
    /// nothing re-asked GitHub and a badge stayed wrong indefinitely.
    ///
    /// The loop resolves **every** (repo, branch) pair in one `gh api graphql` call
    /// (cost 1, independent of repo/worktree/window count), writes the cache the
    /// tree snapshot reads, and bumps the registry's change-notify **only when a
    /// verdict actually moved** — so the server's diff pushes to every open window
    /// exactly when CI state changes, and never otherwise.
    ///
    /// Cadence adapts: [`pr_poll_interval`] (~10 s) while any badge is pending,
    /// doubling to [`MAX_PR_POLL_INTERVAL`] once everything is terminal, and it
    /// polls nothing at all while no window is registered. That is a battery and
    /// wakeup concern rather than a budget one.
    ///
    /// Idempotent, and a no-op outside a tokio runtime (mirroring
    /// [`start_menu_refresh`](Self::start_menu_refresh) and the Snowflake keep-alive
    /// heartbeat), so unit tests build a bare service that never spawns `gh`.
    pub fn start_pr_poller(&self) {
        // Both resolved once at spawn: process-stable env config (the
        // menu-refresh precedent), never re-read per poll.
        self.start_pr_poller_with(pr_poll_interval(), crate::pr_status::resolve_gh_binary());
    }

    /// [`start_pr_poller`](Self::start_pr_poller) with an explicit cadence and
    /// `gh` binary, so tests drive the loop at millisecond speed against a stub
    /// **without mutating the process environment** — one global env var cannot
    /// serve two parallel tests pointing at different fakes. Mirrors the
    /// [`TreeSnapshotCache::with_ttl`] seam and the Snowflake heartbeat's
    /// "interval via config, not env" rule.
    fn start_pr_poller_with(&self, base: Duration, gh_bin: PathBuf) {
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!("no tokio runtime; worktrees PR poller not started");
            return;
        }
        let mut guard = self.poller.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_some() {
            return;
        }
        let token = CancellationToken::new();
        let loop_token = token.clone();
        let registry = self.registry.clone();
        let tree_cache = self.tree_cache.clone();
        let pr_cache = self.pr_cache.clone();
        // Captured here, before the task's first sleep, so a window that registers
        // while the loop is starting still wakes it rather than being missed.
        let mut changes = self.registry.subscribe_changes();
        let handle = tokio::spawn(async move {
            // Two independent cadences. The loop *wakes* every `base` — cheap: a read
            // of the coalescing snapshot cache, no subprocess, no network. It only
            // *asks GitHub* when there is reason to: the watched state moved, or the
            // backoff has elapsed.
            //
            // They have to be separate because the two things that should trigger a
            // fetch arrive by different routes. A window opening bumps the registry's
            // change-notify, but **a commit does not** — nothing in the daemon is
            // notified when you `git push`. The only way to notice is to look, so the
            // loop looks often and cheaply, and pays only when something changed.
            let mut backoff = base;
            let mut last_poll: Option<Instant> = None;
            let mut watched: Option<Vec<PrWatch>> = None;
            loop {
                // Wait first: at startup no window has registered yet, and the
                // first snapshot would be empty anyway.
                tokio::select! {
                    () = loop_token.cancelled() => break,
                    () = tokio::time::sleep(base) => {}
                    // A window opened or closed — look now rather than at the next
                    // tick.
                    result = changes.changed() => {
                        // Unreachable today: this task owns an `Arc` of the registry
                        // that holds the sender, so it cannot be dropped while we are
                        // here. Kept anyway because the alternative is worse — a
                        // closed channel makes `changed()` return `Ready` forever, so
                        // ignoring the error would spin this loop at full speed,
                        // re-snapshotting and re-running `gh` every iteration. One
                        // unreachable line is a cheap guard against that.
                        if result.is_err() {
                            break;
                        }
                    }
                }
                // Off the coalescing snapshot cache, so this reuses the tick's
                // `build_tree` rather than walking git a second time.
                let snapshot = tree_cache.snapshot().await;
                let watch = pr_watch_from_snapshot(&snapshot);
                if watch.is_empty() {
                    // No windows, or nothing on GitHub: ask nothing, and forget any
                    // backoff so the next tree starts fresh rather than inheriting a
                    // ceiling earned on a tree that no longer exists.
                    backoff = base;
                    last_poll = None;
                    watched = None;
                    continue;
                }
                // Did anything we care about move? A new commit shows up here as a
                // changed head — which is exactly the push case the change-notify
                // cannot see.
                let moved = watched.as_ref() != Some(&watch);
                if !pr_should_fetch(moved, last_poll.map(|at| at.elapsed()), backoff) {
                    continue;
                }
                if moved {
                    // Fresh work: watch it closely rather than serving out a backoff
                    // earned while it was quiet.
                    backoff = base;
                }
                let targets: Vec<PrTarget> = watch.iter().map(|w| w.target.clone()).collect();
                // `gh` is a blocking subprocess: never on an async worker. A join
                // failure (the task panicked, or the runtime is going down) folds
                // into the same error channel as a `gh` failure — both mean "no
                // badges this round", and neither deserves its own handling.
                let bin = gh_bin.clone();
                let resolved = tokio::task::spawn_blocking(move || {
                    crate::pr_status::resolve_with(&bin, &targets)
                })
                .await
                .unwrap_or_else(|err| Err(anyhow!("blocking poll task failed: {err}")));
                // Best-effort decoration: a missing/unauthenticated `gh`, a network
                // blip, or a rate limit must never sink the tree. A failed poll
                // leaves the last good badges in place rather than blanking every
                // row, and is not "pending", so it backs off rather than hammers.
                let pending = match resolved {
                    Ok(badges) => {
                        // Bump only on a real change, or the server's diff-and-drop
                        // is defeated and every window re-renders on every poll.
                        if pr_cache.replace(badges) {
                            registry.bump();
                        }
                        pr_cache.any_pending()
                    }
                    Err(err) => {
                        tracing::debug!("PR badge poll failed: {err:#}");
                        false
                    }
                };
                last_poll = Some(Instant::now());
                // Record what this verdict was about, so the *next* tick can tell a
                // genuine change from a quiet tree.
                watched = Some(watch);
                backoff = next_pr_poll_delay(backoff, base, pending);
            }
        });
        *guard = Some(PollerTask { token, handle });
    }

    /// Handles the `close` op: close a worktree's window and, for a **linked**
    /// worktree, delete it. The flow has two phases keyed off `confirmed`:
    ///
    /// - **Phase 1** (`remove:true`, `confirmed:false`) — a pure, side-effect-free
    ///   [`git_safety`] check returning the risks of deleting, so the extension can
    ///   show a modal confirm only when something would actually be lost.
    /// - **Phase 2** (`confirmed:true`, or any `remove:false`) — execute: signal
    ///   the owning window(s) to close, then (for `remove:true`) `git2`-prune the
    ///   worktree. The main working tree is refused defensively.
    ///
    /// Cross-window signalling (another window has the target open) is a
    /// fast-follow: this core handles the **no-window** and **self-close**
    /// (`requester_key == target_key`) cases, and errors clearly when another
    /// window owns the target so the destructive path is never taken blind.
    async fn close(&self, req: CloseRequest) -> Result<Value> {
        // Which live windows currently have the target open. The canonical-path
        // compare is disk I/O, so run it (with the safety check below) on a
        // blocking thread, never under the registry lock or on the async worker.
        let entries = self.registry.list();
        let scan_path = req.path.clone();
        let open_windows =
            tokio::task::spawn_blocking(move || windows_with_path(&entries, &scan_path))
                .await
                .unwrap_or_default();
        let open = !open_windows.is_empty();
        let window_key = open_windows.first().map(|(k, _)| k.clone());
        let window_folder_count = open_windows.first().map_or(0, |(_, c)| *c);

        // Phase 1: the safety check runs only for a delete request awaiting
        // confirmation. A "Close Window" (remove:false) never inspects git and
        // has nothing to confirm, so it skips straight to execute.
        if req.remove && !req.confirmed {
            let path = req.path.clone();
            let git = tokio::task::spawn_blocking(move || git_safety(&path))
                .await
                .map_err(|e| anyhow!("safety check task panicked: {e}"))??;
            return Ok(serde_json::to_value(SafetyReport {
                removable: git.removable,
                is_main: git.is_main,
                open,
                window_key,
                window_folder_count,
                risks: git.risks,
                info: git.info,
            })
            .unwrap_or_else(|_| json!({})));
        }

        // Phase 2: execute. Signal every owning window *other than the
        // requester* (which closes itself on our `ok:true` reply, avoiding the
        // ext-host-dies-mid-op race) and wait for each to unregister before
        // touching the worktree. The directive reaches a cross-window target via
        // its heartbeat reply — the only channel the daemon has to a window it
        // can reply to but never call.
        let others: Vec<String> = open_windows
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| req.requester_key.as_deref() != Some(k))
            .collect();
        for key in &others {
            self.registry.mark_close_pending(key);
        }
        if !others.is_empty() {
            await_windows_closed(
                &self.registry,
                &req.path,
                req.requester_key.as_deref(),
                CLOSE_WAIT_TIMEOUT,
                CLOSE_WAIT_POLL,
            )
            .await?;
        }

        if req.remove {
            let path = req.path.clone();
            // Taken *after* the wait above, so concurrent executes still overlap
            // their heartbeat waits (#1359) and only the prune itself serializes.
            // Load-bearing placement, not incidental: hoisting this above
            // `await_windows_closed` would restack the waits and undo the whole
            // point. Pinned by `concurrent_closes_overlap_their_heartbeat_waits`.
            let _guard = self.prune_lock.lock().await;
            tokio::task::spawn_blocking(move || remove_worktree(&path))
                .await
                .map_err(|e| anyhow!("worktree removal task panicked: {e}"))??;
            Ok(json!({ "removed": true }))
        } else {
            // "Close Window" with no owning window is a no-op success; a
            // self-close replies and the extension closes its own window.
            Ok(json!({ "closed": true }))
        }
    }
}

impl Default for WorktreesService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DaemonService for WorktreesService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    async fn handle(&self, op: &str, payload: Value) -> Result<Value> {
        match op {
            "register" => {
                let req: RegisterRequest =
                    serde_json::from_value(payload).context("invalid `register` payload")?;
                if req.key.trim().is_empty() {
                    bail!("`register` requires a non-empty `key`");
                }
                self.registry.register(req);
                Ok(json!({ "ok": true }))
            }
            "heartbeat" => {
                let key = require_str(&payload, "key", "heartbeat")?;
                let known = self.registry.heartbeat(key);
                // A pending close directive (#1277) rides the reply as an
                // additive `close` field, taken-and-cleared here so it fires
                // exactly once. Omitted when false to keep older windows — which
                // read only `known` — byte-identical on the wire.
                let mut reply = json!({ "known": known });
                if self.registry.take_close_pending(key) {
                    reply["close"] = Value::Bool(true);
                }
                Ok(reply)
            }
            "unregister" => {
                let key = require_str(&payload, "key", "unregister")?;
                Ok(json!({ "removed": self.registry.unregister(key) }))
            }
            "list" => Ok(json!({ "windows": enriched_windows(self.registry.list()).await })),
            "tree" => {
                // The same `{ repos, show_closed }` snapshot the `subscribe`
                // stream pushes, so a one-shot `tree` fetch and the live stream
                // agree byte-for-byte (the git enumeration runs off-lock on a
                // blocking thread inside the helper). Computed fresh here — the
                // `tree` op is a rare manual refresh, deliberately bypassing the
                // stream's coalescing cache so it never returns a stale view
                // (#1303).
                Ok(tree_snapshot(&self.registry, self.pr_cache.clone()).await)
            }
            "ahead-behind" => {
                // Lazy per-worktree divergence (#1306). The `tree`/`subscribe`
                // snapshot no longer carries ahead/behind — the dominant
                // per-worktree cost when computed eagerly every tick — so a client
                // (the extension on expand, `worktrees tree`) asks for it here only
                // for the worktrees it is about to show. Batched by path, one op per
                // repo expand; the git walks run on a blocking thread.
                let paths = payload
                    .get("paths")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(PathBuf::from)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Ok(json!({ "results": ahead_behind_results(paths).await }))
            }
            "set-show-closed" => {
                // The daemon-backed show/hide-closed toggle (#1301). Setting it
                // bumps the change-notify, so every subscribed window re-pushes a
                // snapshot carrying the new `show_closed` — reliable cross-window
                // sync `context.globalState` could not do.
                let show_closed = payload
                    .get("show_closed")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| anyhow!("`set-show-closed` requires a boolean `show_closed`"))?;
                self.registry.set_show_closed(show_closed);
                Ok(json!({ "ok": true }))
            }
            "open" => {
                // Focus (or open — VS Code reuses an already-open window) an
                // arbitrary worktree folder supplied by a socket client, reusing
                // the tray's launcher path: `focus_window` resolves the launcher
                // (`OMNI_DEV_VSCODE_BIN` → well-known paths → `code`) and applies
                // the absolute-existing-directory guard (which also blocks a
                // `-`-leading path being parsed by `code` as a flag). This is the
                // one op a socket *writer* can use to spawn `code`; see the
                // ADR-0040 threat model (#1266).
                let path = require_str(&payload, "path", "open")?;
                focus_window(Path::new(path))?;
                Ok(json!({ "ok": true }))
            }
            "close" => {
                // Close a worktree's window and (for a linked worktree)
                // **delete** it. Destructive, so all git logic stays in the
                // daemon (git2, never a shell) and the main working tree is
                // refused defensively — the UI gating is not the only guard.
                // See ADR-0049 and docs/worktrees-service.md.
                let req: CloseRequest =
                    serde_json::from_value(payload).context("invalid `close` payload")?;
                self.close(req).await
            }
            other => bail!("unknown worktrees op: {other}"),
        }
    }

    fn subscribe(&self, op: &str, _payload: &Value) -> Option<Box<dyn ServiceStream>> {
        // The single streaming op: a live push of the repo/worktree `tree`
        // snapshot. Every other op falls through to the request→reply `handle`.
        if op != "subscribe" {
            return None;
        }
        Some(Box::new(WorktreesStream {
            // Every stream reads through the one shared cache, so N windows
            // sampling the same tick build the tree once, not N times (#1303).
            cache: self.tree_cache.clone(),
            // Capture the change source *now* — before the server takes its
            // initial snapshot — so a change racing that snapshot still wakes us.
            changes: self.registry.subscribe_changes(),
        }))
    }

    fn menu(&self) -> MenuSnapshot {
        // Serve the snapshot the background task maintains off the main thread;
        // fall back to a one-off inline compute only before the first refresh
        // lands (or with no runtime — the unit tests). Never blocks on git here
        // in the daemon, honouring the trait's "cheap, must not block" contract.
        let cached = self
            .menu_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let items = cached.unwrap_or_else(|| menu_items_for(&self.registry.list()));
        MenuSnapshot {
            title: SUBMENU_TITLE.to_string(),
            items,
        }
    }

    async fn menu_action(&self, action_id: &str) -> Result<()> {
        if let Some(key) = action_id.strip_prefix("focus:") {
            // The registry resolves the folder under its own lock and clones it
            // out, so the mutex is never held across the process launch.
            let folder = self
                .registry
                .first_folder(key)
                .ok_or_else(|| anyhow!("no open window with key {key} (it may have closed)"))?;
            focus_window(&folder)?;
            return Ok(());
        }
        bail!("unknown worktrees menu action: {action_id}")
    }

    async fn status(&self) -> ServiceStatus {
        let entries = self.registry.list();
        let repos: BTreeSet<&str> = entries.iter().filter_map(|e| e.repo.as_deref()).collect();
        let summary = format!("{} window(s) across {} repo(s)", entries.len(), repos.len());
        let windows = enriched_windows(entries).await;
        ServiceStatus {
            name: SERVICE_NAME.to_string(),
            healthy: true,
            summary,
            detail: json!({ "windows": windows }),
        }
    }

    async fn shutdown(&self) {
        // Stop the background menu-refresh task; the registry itself is in-memory
        // with nothing to drain or persist. Take the task out from under the lock
        // first so the `std::Mutex` is never held across the `.await`.
        let task = self
            .refresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.token.cancel();
            let _ = task.handle.await;
        }
        // Same discipline for the PR badge poller (#1337): take it out from under
        // its lock before awaiting, so no `std::Mutex` is held across the `.await`.
        let poller = self
            .poller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(poller) = poller {
            poller.token.cancel();
            let _ = poller.handle.await;
        }
    }
}

/// Extracts a required string `field` from an op payload, erroring with the op
/// name when it is absent or not a string. Shared by `heartbeat`/`unregister`
/// (`key`) and `open` (`path`).
fn require_str<'a>(payload: &'a Value, field: &str, op: &str) -> Result<&'a str> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("`{op}` requires `{field}`"))
}

/// The live git state of a worktree folder: the checked-out branch and how far
/// it has diverged from its upstream. Computed on read from the on-disk repo
/// (#1186), so `list`/`status`/`menu` reflect the current branch rather than a
/// snapshot taken at registration.
///
/// Every field is optional and degrades independently: a folder that is not a
/// git repo, is on a detached HEAD, or whose branch tracks no upstream is still
/// listed — just without the fields it cannot supply. The `skip_serializing_if`
/// attributes let it flatten cleanly onto an entry (see [`EnrichedEntry`]),
/// omitting each absent field on the wire.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
struct GitStatus {
    /// The checked-out branch, or `None` when detached or not in a repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// The commit HEAD points at, or `None` when unborn or not in a repo. Present
    /// even on a detached HEAD, which has a commit but no branch. Rides the
    /// streamed snapshot so a new commit is a real delta the server's diff cannot
    /// drop — without it, a push serialises byte-identically and no client
    /// re-renders (#1337).
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
    /// The commit the branch's configured upstream ref points at, or `None`
    /// without an upstream (or when detached, unborn, or not in a repo). Rides
    /// the streamed snapshot for the same reason as `head_sha`, one ref over: a
    /// **push** moves only `refs/remotes/<remote>/<branch>`, leaving every other
    /// field — `head_sha` included — byte-identical, so without this the frame
    /// serialised the same, the server's diff dropped it, and the lazily-fetched
    /// ahead/behind was never re-asked (#1344).
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_sha: Option<String>,
    /// Commits the branch is ahead of its upstream (`None` without an upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    ahead: Option<usize>,
    /// Commits the branch is behind its upstream (`None` without an upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    behind: Option<usize>,
    /// The main repository's directory name — the parent repo for a linked
    /// worktree, the checkout's own directory otherwise. Derived from git's
    /// common dir so a worktree names the repo it belongs to rather than its
    /// worktree-folder basename. `None` when not in a repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    main_repo: Option<String>,
    /// Whether the enriched folder is a **linked** git worktree rather than the
    /// repository's main working tree. Omitted (false) for a normal checkout.
    #[serde(skip_serializing_if = "is_false")]
    is_worktree: bool,
}

/// `skip_serializing_if` predicate for a `bool` defaulting to `false`, so the
/// field is dropped on the wire unless set — keeping older clients byte-identical
/// (the protocol's forward-compatibility convention).
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Computes the **full** [`GitStatus`] of `folder` — branch, repo identity, and
/// the ahead/behind divergence from upstream. Used by the one-shot `list`/`status`
/// op and the tray menu, both bounded to the (few) open windows, where the extra
/// `graph_ahead_behind` walk is negligible. The streamed `tree` snapshot uses the
/// cheaper [`git_status_cheap`] instead and fetches divergence on demand (#1306).
fn git_status(folder: &Path) -> GitStatus {
    git_status_impl(folder, true)
}

/// Computes the **cheap** [`GitStatus`] of `folder` — branch and repo identity
/// only, skipping the (expensive) `graph_ahead_behind` upstream revwalk. Used by
/// the `tree`/`subscribe` snapshot, which is rebuilt for **every** worktree on
/// **every** tick: divergence there is computed lazily via the `ahead-behind` op
/// only for the worktrees a client actually looks at (#1306). The `ahead`/`behind`
/// fields stay `None`, so they are omitted on the wire exactly as for a branch
/// with no upstream.
fn git_status_cheap(folder: &Path) -> GitStatus {
    git_status_impl(folder, false)
}

/// The shared body of [`git_status`] / [`git_status_cheap`]: discovers the
/// repository that contains `folder` — so a subdirectory or a linked worktree both
/// resolve — reads HEAD, and (only when `with_ahead_behind`) walks the upstream
/// divergence. Every failure mode degrades to an empty status rather than
/// erroring: the enrichment is best-effort and must never sink a `list` or a tree.
fn git_status_impl(folder: &Path, with_ahead_behind: bool) -> GitStatus {
    let Ok(repo) = Repository::discover(folder) else {
        return GitStatus::default();
    };
    // Repo identity applies even when HEAD is unborn or detached, so a worktree
    // still names its parent repo (and is flagged as a worktree) in those states.
    let base = GitStatus {
        main_repo: main_repo_name(repo.commondir()),
        is_worktree: repo.is_worktree(),
        ..GitStatus::default()
    };
    let Ok(head) = repo.head() else {
        // An unborn branch (fresh repo, no commits) or an unreadable HEAD.
        return base;
    };
    // Resolved here — before the branch filter below, so a detached HEAD still
    // reports its commit, and before `Branch::wrap` consumes `head`. `target()` is
    // a refs read: no revwalk and no object lookup, so unlike the divergence walk
    // it is cheap enough for the streamed snapshot's every-worktree-every-tick
    // rebuild (#1306's bar).
    let base = GitStatus {
        head_sha: head.target().map(|oid| oid.to_string()),
        ..base
    };
    // A branch HEAD has a UTF-8 shorthand; anything else — a detached HEAD
    // (mid-rebase or a checked-out tag/commit), or the rare non-UTF-8 branch
    // name — degrades to no branch through this one path.
    let Some(name) = head
        .shorthand()
        .ok()
        .filter(|_| head.is_branch())
        .map(str::to_string)
    else {
        return base;
    };
    // Consumes `head`, so it has to follow the `shorthand()` read above. A pure
    // type wrapper — no I/O — so hoisting it out of the `with_ahead_behind` arm
    // below costs the cheap path nothing, and is what gives it a handle to
    // resolve the upstream from.
    let branch = git2::Branch::wrap(head);
    // Unlike the divergence walk, this rides both paths: it is what makes a push
    // a visible delta (#1344).
    let upstream_sha = upstream_target(&branch);
    // The divergence walk is the dominant per-worktree cost, so the cheap path
    // skips it and leaves ahead/behind absent.
    let (ahead, behind) = if with_ahead_behind {
        match upstream_ahead_behind(&repo, &branch) {
            Some((ahead, behind)) => (Some(ahead), Some(behind)),
            None => (None, None),
        }
    } else {
        (None, None)
    };
    GitStatus {
        branch: Some(name),
        upstream_sha,
        ahead,
        behind,
        ..base
    }
}

/// The commit `branch`'s configured upstream ref points at, or `None` when it
/// tracks no upstream (or the ref is unresolvable).
///
/// Costs a config lookup (`branch.<name>.remote` + `.merge`) and a
/// remote-tracking refs read — more than [`git_status_impl`]'s single `head`
/// refs read, but still **no revwalk and no object lookup**, which is the bar
/// #1306 set for the snapshot's every-worktree-every-tick rebuild and the one
/// `graph_ahead_behind` fails. [`upstream_ahead_behind`] already resolves the
/// same OID, so it is proven reachable.
fn upstream_target(branch: &git2::Branch<'_>) -> Option<String> {
    Some(branch.upstream().ok()?.get().target()?.to_string())
}

/// The ahead/behind divergence of `folder`'s checked-out branch versus its
/// upstream, computed on demand for the lazy `ahead-behind` op (#1306). Mirrors the
/// branch resolution in [`git_status_impl`] but does **only** the upstream walk
/// [`git_status_cheap`] omits. `None` when `folder` is not a repo, is on a detached
/// or unborn HEAD, or tracks no upstream — every case the tree renders without a
/// sync indicator.
fn folder_ahead_behind(folder: &Path) -> Option<(usize, usize)> {
    let repo = Repository::discover(folder).ok()?;
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    let branch = git2::Branch::wrap(head);
    upstream_ahead_behind(&repo, &branch)
}

/// The main repository's directory name from git's common dir. For the usual
/// `<repo>/.git` layout — shared by a checkout and all its linked worktrees —
/// that is the working-tree directory's name; for a bare repo (`<name>.git`) it
/// is that directory with a trailing `.git` stripped. Best-effort: `None` when
/// no name can be derived.
fn main_repo_name(commondir: &Path) -> Option<String> {
    let file_name = commondir.file_name()?.to_string_lossy().into_owned();
    if file_name == ".git" {
        // Normal layout: the repo is the directory that contains `.git`.
        commondir
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
    } else {
        // A bare repo: use its own directory name, without any `.git` suffix.
        Some(
            file_name
                .strip_suffix(".git")
                .unwrap_or(&file_name)
                .to_string(),
        )
    }
}

/// Ahead/behind commit counts of `branch` versus its configured upstream, or
/// `None` when the branch tracks no upstream (or either tip is unresolvable).
fn upstream_ahead_behind(repo: &Repository, branch: &git2::Branch<'_>) -> Option<(usize, usize)> {
    let upstream = branch.upstream().ok()?;
    let local_oid = branch.get().target()?;
    let upstream_oid = upstream.get().target()?;
    repo.graph_ahead_behind(local_oid, upstream_oid).ok()
}

/// The wire shape of an enriched window: the stored entry fields plus the
/// daemon-computed git state, flattened into one JSON object. Serializing
/// through a single struct (rather than mutating a `Value`) keeps every present
/// field on one code path and lets `skip_serializing_if` on [`GitStatus`] drop
/// the absent git fields — no manual per-field insertion.
#[derive(Serialize)]
struct EnrichedEntry<'a> {
    #[serde(flatten)]
    entry: &'a WindowEntry,
    #[serde(flatten)]
    git: GitStatus,
}

/// Serializes a registry entry and folds in the live [`git_status`] of its
/// primary (first) folder, producing the JSON object served on the wire
/// (`list`/`status`) and read by the extension UI. Only the primary folder is
/// enriched — it is the one the table shows and the "focus" action opens.
fn enriched_entry(entry: &WindowEntry) -> Value {
    let git = entry
        .folders
        .first()
        .map(|folder| git_status(folder))
        .unwrap_or_default();
    serde_json::to_value(EnrichedEntry { entry, git }).unwrap_or_else(|_| json!({}))
}

/// Enriches a batch of entries with their git state on a blocking thread, since
/// `git2` does synchronous disk I/O and this runs inside the async control-socket
/// handler. A join failure degrades to an empty list rather than erroring.
async fn enriched_windows(entries: Vec<WindowEntry>) -> Vec<Value> {
    tokio::task::spawn_blocking(move || entries.iter().map(enriched_entry).collect())
        .await
        .unwrap_or_default()
}

// --- Repo/worktree tree (#1265) ----------------------------------------------

/// A GitHub `owner/name` identity parsed from a repository's `origin` remote.
/// Present on a repo in the `tree` payload only for `github.com` remotes; a
/// non-GitHub (or remote-less) repo omits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GithubIdentity {
    /// The repository owner (user or org) — the first path segment.
    owner: String,
    /// The repository name, with any `.git` suffix stripped.
    name: String,
}

/// One worktree of a repository in the `tree` payload: its path, live git state,
/// whether it is the main working tree, and whether a VS Code window currently
/// has it open (with that window's key, for the focus action). Optional git
/// fields degrade independently, exactly like [`GitStatus`].
///
/// Ahead/behind **divergence** is deliberately absent from this snapshot: it was
/// the dominant per-worktree cost when computed eagerly for every worktree on
/// every tick, so it is now fetched lazily via the `ahead-behind` op only for the
/// worktrees a client actually shows (#1306).
///
/// The two **OIDs** the divergence is computed from — `head_sha` and
/// `upstream_sha` — do ride the snapshot, which is not a contradiction: each is a
/// refs read rather than a commit-graph walk, and between them they are what makes
/// a commit (#1337) or a push (#1344) a *visible delta*, so a client knows to
/// re-ask for the counts it left behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct TreeWorktree {
    /// Absolute path to the worktree's working directory.
    path: String,
    /// The checked-out branch, or `None` when detached or unborn.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// The commit HEAD points at, or `None` when unborn. Unlike ahead/behind this
    /// **does** ride the snapshot: it costs a refs read, and it is what makes a new
    /// commit a visible delta, so a push re-renders instead of being dropped by the
    /// server's snapshot diff (#1337).
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
    /// The commit the branch's upstream ref points at, or `None` without an
    /// upstream. The push counterpart of `head_sha`: a push moves only
    /// `refs/remotes/<remote>/<branch>`, so this is the *one* field that moves —
    /// making the snapshot a real delta the server's diff cannot drop, which is
    /// what re-fetches the lazy ahead/behind (#1344).
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_sha: Option<String>,
    /// Whether this is the repository's main working tree (vs a linked worktree).
    is_main: bool,
    /// Whether a live VS Code window currently has this worktree open.
    open: bool,
    /// The open window's registry key, when `open` — the handle a focus action
    /// resolves. Absent for a worktree with no open window.
    #[serde(skip_serializing_if = "Option::is_none")]
    window_key: Option<String>,
    /// The open PR whose head is this worktree's branch, with its CI verdict
    /// (#1337). Resolved by the daemon's background poller and folded on as the
    /// snapshot is built, so every open window sees the same live state without
    /// each running its own `gh`. Absent for a detached/non-GitHub worktree, one
    /// with no open PR, or until the first poll lands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pr: Option<PrBadge>,
}

/// One repository (with **all** its worktrees) in the `tree` payload. Repos are
/// derived from the distinct open windows; a repo leaves the tree when its last
/// window closes (the open-window-derived model, ADR-0040 / #1264).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct TreeRepo {
    /// The main repository's directory name (see [`main_repo_name`]).
    main_repo: String,
    /// The GitHub identity of `origin`, when it is a `github.com` remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    github: Option<GithubIdentity>,
    /// Absolute path to the main working tree — the repo's root.
    root: String,
    /// Every worktree of the repo: the main working tree first, then linked
    /// worktrees sorted by path.
    worktrees: Vec<TreeWorktree>,
}

/// Parses a git remote URL into its GitHub `owner/name`, or `None` for any
/// non-GitHub host. Handles the common forms: `https://github.com/o/r(.git)`,
/// `http://…`, `ssh://git@github.com/o/r(.git)`, `git://github.com/o/r(.git)`,
/// and the SCP-like `git@github.com:o/r(.git)`. A trailing `.git` and trailing
/// slashes are stripped; anything with an empty or extra path segment is
/// rejected (best-effort, never panics).
fn github_identity(url: &str) -> Option<GithubIdentity> {
    let url = url.trim();
    // Reduce every supported form to the `owner/name…` tail after the host.
    let rest = [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git://github.com/",
        "git@github.com:",
    ]
    .iter()
    .find_map(|prefix| url.strip_prefix(prefix))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    // A well-formed identity has exactly two non-empty segments.
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(GithubIdentity {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// The GitHub identity of `repo`: `origin`'s URL first, else the first
/// `github.com` remote found. `None` when no remote is a GitHub remote.
fn remote_github_identity(repo: &Repository) -> Option<GithubIdentity> {
    if let Ok(origin) = repo.find_remote("origin") {
        if let Some(id) = origin.url().ok().and_then(github_identity) {
            return Some(id);
        }
    }
    // `remotes()` yields `Result<Option<&str>, _>` per name; the first flatten
    // drops the (per-name) errors, the second the non-UTF-8 `None`s. `names` is
    // bound so `iter()` can borrow it (only `&StringArray` is `IntoIterator`).
    let names = repo.remotes().ok();
    names
        .iter()
        .flat_map(|arr| arr.iter())
        .flatten()
        .flatten()
        .filter_map(|name| repo.find_remote(name).ok())
        .find_map(|remote| remote.url().ok().and_then(github_identity))
}

/// Canonicalizes a path for stable comparison (resolving symlinks and `..`),
/// falling back to the path as-given when it cannot be canonicalized (e.g. it
/// no longer exists) so the join still degrades gracefully.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Indexes the open windows by canonicalized workspace-folder path → window key,
/// so a worktree path can be joined back to the window (if any) that has it open.
/// The first window wins a shared folder; `entries` arrive in a deterministic
/// (repo, key) order, so the choice is stable.
fn open_window_index(entries: &[WindowEntry]) -> HashMap<PathBuf, String> {
    let mut index = HashMap::new();
    for entry in entries {
        for folder in &entry.folders {
            index
                .entry(canonical(folder))
                .or_insert_with(|| entry.key.clone());
        }
    }
    index
}

/// Builds a [`TreeWorktree`] for `path`: reuses [`git_status_cheap`] for the live
/// git state (branch + repo identity, **no** ahead/behind walk — that is lazy per
/// #1306) and joins the open-window index for `open`/`window_key`. `is_main` is set
/// by the caller from the enumeration (main working tree vs linked).
fn worktree_entry(
    path: &Path,
    is_main: bool,
    open_index: &HashMap<PathBuf, String>,
) -> TreeWorktree {
    let status = git_status_cheap(path);
    let window_key = open_index.get(&canonical(path)).cloned();
    TreeWorktree {
        path: path.display().to_string(),
        branch: status.branch,
        head_sha: status.head_sha,
        upstream_sha: status.upstream_sha,
        is_main,
        open: window_key.is_some(),
        window_key,
        // Folded on afterwards by `fold_pr_badges`, which needs the repo's GitHub
        // identity — known one level up, in `repo_tree`.
        pr: None,
    }
}

/// Folds the poller's cached PR badges onto each worktree of each repo (#1337).
///
/// Runs after [`build_tree`] because a badge is keyed by (repo GitHub identity,
/// branch) and the identity is only known once the repo is assembled. Purely a
/// cache read — no I/O, no network — so it is safe on the snapshot's hot path. A
/// non-GitHub repo, a branchless worktree, or an unresolved branch simply keeps
/// `pr: None` and renders nothing.
///
/// A verdict computed for a **different commit** than the worktree has checked out
/// is downgraded to pending here rather than shown as-is. That is what makes a push
/// invalidate the badge the moment it happens: the cache still holds the previous
/// commit's verdict, and this fold — which runs on every snapshot — notices without
/// waiting for a poll. Without it the previous head's `✓` stands until the poller
/// next runs, which is up to the full backoff.
fn fold_pr_badges(repos: &mut [TreeRepo], pr_cache: &PrStatusCache) {
    for repo in repos {
        let Some(github) = repo.github.clone() else {
            continue;
        };
        for worktree in &mut repo.worktrees {
            let Some(branch) = &worktree.branch else {
                continue;
            };
            let Some(mut badge) = pr_cache.get(&github.owner, &github.name, branch) else {
                continue;
            };
            if badge.is_stale_for(worktree.head_sha.as_deref()) {
                badge.checks = PrCheckState::Pending;
            }
            worktree.pr = Some(badge);
        }
    }
}

/// Enumerates a repository and all its worktrees into a [`TreeRepo`], given a
/// handle discovered from one of its folders. Opens the **main** repo from the
/// shared common dir's parent so the main working tree and every linked worktree
/// are enumerated regardless of which one seeded the discovery. `None` for a
/// bare or otherwise root-less repo (no working tree to show).
fn repo_tree(discovered: &Repository, open_index: &HashMap<PathBuf, String>) -> Option<TreeRepo> {
    // The common dir (`…/<root>/.git`) is shared by the main checkout and all
    // linked worktrees; its parent is the main working tree.
    let commondir = canonical(discovered.commondir());
    let main_root = commondir.parent()?.to_path_buf();
    let main_repo = Repository::open(&main_root).ok()?;

    // Main working tree first.
    let mut worktrees = vec![worktree_entry(&main_root, true, open_index)];
    // Then every linked worktree, sorted by path for deterministic output. The
    // `StringArray` of names is bound so `iter()` can borrow it (only
    // `&StringArray` is `IntoIterator`); a name that no longer resolves to a
    // worktree is skipped.
    let names = main_repo.worktrees().ok();
    let mut linked: Vec<PathBuf> = names
        .iter()
        .flat_map(|arr| arr.iter())
        .flatten() // Result<Option<&str>, _> → Option<&str> (drop per-name errors)
        .flatten() // Option<&str> → &str (drop non-UTF-8 names)
        .filter_map(|name| main_repo.find_worktree(name).ok())
        .map(|wt| wt.path().to_path_buf())
        .collect();
    linked.sort();
    worktrees.extend(
        linked
            .iter()
            .map(|path| worktree_entry(path, false, open_index)),
    );

    Some(TreeRepo {
        main_repo: main_repo_name(&commondir)?,
        github: remote_github_identity(&main_repo),
        root: main_root.display().to_string(),
        worktrees,
    })
}

/// Resolves the seed `folders` to their distinct repositories and enumerates
/// each repo's worktrees. Dedupes repos by their common dir (shared across a
/// repo's worktrees) via a `BTreeMap` for deterministic ordering; a folder that
/// is not in a git repo is skipped. Pure blocking git I/O — call it via
/// [`tree_repos`], never under the registry lock.
fn build_tree(folders: Vec<PathBuf>, windows: Vec<WindowEntry>) -> Vec<TreeRepo> {
    let open_index = open_window_index(&windows);
    let mut repos: BTreeMap<PathBuf, TreeRepo> = BTreeMap::new();
    for folder in &folders {
        let Ok(repo) = Repository::discover(folder) else {
            continue;
        };
        let key = canonical(repo.commondir());
        if repos.contains_key(&key) {
            continue;
        }
        if let Some(tree) = repo_tree(&repo, &open_index) {
            repos.insert(key, tree);
        }
    }
    repos.into_values().collect()
}

/// Enumerates and enriches the repo/worktree tree on a blocking thread (`git2`
/// does synchronous disk I/O and this runs inside the async control-socket
/// handler), returning the serialized `repos` array. A join failure degrades to
/// an empty list rather than erroring, matching [`enriched_windows`].
async fn tree_repos(
    folders: Vec<PathBuf>,
    windows: Vec<WindowEntry>,
    pr_cache: Arc<PrStatusCache>,
) -> Vec<Value> {
    tokio::task::spawn_blocking(move || {
        let mut repos = build_tree(folders, windows);
        fold_pr_badges(&mut repos, &pr_cache);
        repos
            .iter()
            .map(|repo| serde_json::to_value(repo).unwrap_or_else(|_| json!({})))
            .collect()
    })
    .await
    .unwrap_or_default()
}

// --- Lazy ahead/behind (#1306) -----------------------------------------------

/// Computes the ahead/behind divergence for a batch of worktree `paths` on demand,
/// returning a JSON object keyed by the **requested** path string:
/// `{ "<path>": { "ahead": n, "behind": m }, … }`. A path with no upstream (or that
/// is not a repo / is detached) is **omitted** — the client renders it without a
/// sync indicator, exactly as the tree does for an absent `ahead`/`behind`.
///
/// Backs the `ahead-behind` op, which exists precisely so the streamed `tree`
/// snapshot can stay cheap: a client fetches divergence only for the worktrees it
/// shows (the extension on expand), not for every worktree on every tick. The git
/// walks are blocking disk I/O, so they run on a blocking thread; a join failure
/// degrades to an empty object rather than erroring.
async fn ahead_behind_results(paths: Vec<PathBuf>) -> Value {
    tokio::task::spawn_blocking(move || {
        let mut results = serde_json::Map::new();
        for path in paths {
            if let Some((ahead, behind)) = folder_ahead_behind(&path) {
                results.insert(
                    path.display().to_string(),
                    json!({ "ahead": ahead, "behind": behind }),
                );
            }
        }
        Value::Object(results)
    })
    .await
    .unwrap_or_else(|_| json!({}))
}

// --- Push subscription (#1267) -----------------------------------------------

/// The [`ServiceStream`] backing the worktrees `subscribe` op: a live push of
/// the same `{ repos: [...] }` snapshot the `tree` op returns (#1265). The
/// server drives it — awaiting [`changed`](ServiceStream::changed) plus its own
/// periodic tick, then diffing [`snapshot`](ServiceStream::snapshot) — so this
/// type only has to (a) relay the registry's change-notify and (b) read the
/// tree snapshot on demand.
///
/// Every window's stream shares one [`TreeSnapshotCache`] (#1303): the snapshot
/// is built at most once per tick and fanned out, rather than each stream
/// rebuilding the identical tree. This type holds only cheap handles — a clone
/// of the shared cache and its own change-notify receiver.
struct WorktreesStream {
    /// The shared coalescing cache the snapshot is read through, so every
    /// stream's tick/change re-sample hits one shared `build_tree` (#1303).
    cache: Arc<TreeSnapshotCache>,
    /// Wakes on each visible-set change (a `register`, a removing `unregister`,
    /// or a mutation-driven reap). A burst coalesces into one wakeup; the
    /// server's diff drops any snapshot that ends up identical.
    changes: watch::Receiver<u64>,
}

#[async_trait]
impl ServiceStream for WorktreesStream {
    async fn changed(&mut self) {
        // `watch::Receiver::changed` marks the newest version seen, so a burst of
        // bumps collapses into a single wakeup. If every sender is gone (the
        // registry — and thus the daemon — is tearing down) it returns `Err`;
        // park instead of returning, so this arm can never spin the server's
        // `select!` (the tick and shutdown arms still drive teardown).
        if self.changes.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    async fn snapshot(&self) -> Value {
        // Read through the shared coalescing cache. The value is built by the
        // same `tree_snapshot` the `tree` op runs, so a one-shot fetch and this
        // live push agree byte-for-byte — but here it is built once per tick and
        // shared across every subscriber rather than rebuilt per stream (#1303).
        self.cache.snapshot().await
    }
}

/// A coalescing cache for the global tree snapshot (#1303).
///
/// Every open VS Code window holds one persistent [`WorktreesStream`], and the
/// server re-samples each on its own `STREAM_TICK` and on every registry change
/// — so with N windows the *identical* global tree was being built N times per
/// tick. This cache collapses that to **one** build: all streams share it, and
/// it rebuilds at most once per `ttl` (the stream tick) per registry
/// change-generation.
///
/// Two conditions gate reuse, and **both** must hold, so freshness is preserved
/// exactly as before:
/// - the registry's [`change_generation`](WorktreesRegistry::change_generation)
///   still matches — a `register`/`unregister`/toggle bumps it and forces a
///   fresh build, so subscribers never see a stale visible set; and
/// - the cached value is younger than `ttl` — so a pure on-disk git change (a
///   branch switch, new commits), which fires no registry event, still surfaces
///   within one tick.
///
/// Concurrency is single-flight: the `.await`-held [`AsyncMutex`] serializes
/// callers, so a burst of N streams waking on the same tick/change performs one
/// build while the rest wait and read the shared result. The one-shot `tree` op
/// bypasses this and computes fresh — it is a rare manual refresh, not part of
/// the per-tick fan-out.
struct TreeSnapshotCache {
    /// The registry every snapshot is built from, and whose change-generation
    /// gates cache reuse.
    registry: Arc<WorktreesRegistry>,
    /// PR badges folded onto each worktree as the snapshot is built (#1337).
    /// Written by the background poller; read here. A miss simply omits `pr`.
    pr_cache: Arc<PrStatusCache>,
    /// How long a built snapshot stays fresh before a tick-driven read rebuilds
    /// it. Defaults to the server's `STREAM_TICK` (via [`new`](Self::new)) so the
    /// coalesced build runs at most once per tick; tests inject a shorter value.
    ttl: Duration,
    /// The single-flight guard and cached result. A `tokio` mutex (not `std`)
    /// because it is deliberately held across the `.await` of the git
    /// enumeration, so concurrent callers serialize onto one build rather than
    /// each computing their own.
    state: AsyncMutex<Option<CachedTree>>,
    /// How many times the tree was actually (re)built — so tests can assert the
    /// coalescing collapses an N-stream burst into one build. Cheap and always
    /// maintained; only read under `#[cfg(test)]`.
    computes: AtomicU64,
}

/// One cached tree snapshot: the shared value plus the two freshness stamps
/// [`TreeSnapshotCache`] checks before reusing it.
struct CachedTree {
    /// The registry change-generation captured *before* the build, so a change
    /// racing the build advances the generation and the next read rebuilds
    /// (conservative: it may rebuild once needlessly, but never serves stale).
    generation: u64,
    /// When the value was built, for the `ttl` staleness check.
    computed_at: Instant,
    /// The already-built `{ repos, show_closed }` snapshot, fanned out to every
    /// subscriber by cloning the `Arc`'s inner value.
    value: Arc<Value>,
}

impl TreeSnapshotCache {
    /// Creates a cache over `registry` with the default TTL — the server's
    /// [`stream_tick`](crate::daemon::server::stream_tick), so the coalesced
    /// build runs at most once per tick.
    fn new(registry: Arc<WorktreesRegistry>, pr_cache: Arc<PrStatusCache>) -> Self {
        Self::with_ttl(registry, pr_cache, crate::daemon::server::stream_tick())
    }

    /// Creates a cache with an explicit `ttl`, for tests that need a short (or
    /// long) freshness window without waiting a real tick.
    fn with_ttl(
        registry: Arc<WorktreesRegistry>,
        pr_cache: Arc<PrStatusCache>,
        ttl: Duration,
    ) -> Self {
        Self {
            registry,
            pr_cache,
            ttl,
            state: AsyncMutex::new(None),
            computes: AtomicU64::new(0),
        }
    }

    /// The current tree snapshot, built at most once per `ttl` per registry
    /// change-generation and shared across all callers. See the type docs for
    /// the freshness and single-flight semantics.
    async fn snapshot(&self) -> Value {
        // Hold the lock across the whole check-and-build so concurrent callers
        // serialize onto one build (single-flight); reading the generation here
        // (before the build) means a change racing the build forces the *next*
        // read to rebuild rather than serving this now-stale value.
        let mut state = self.state.lock().await;
        let generation = self.registry.change_generation();
        // Reuse the cached value only while it matches the current generation
        // *and* is within the TTL; either failing forces a rebuild.
        let fresh = state.as_ref().and_then(|cached| {
            (cached.generation == generation && cached.computed_at.elapsed() < self.ttl)
                .then(|| Arc::clone(&cached.value))
        });
        let value = if let Some(value) = fresh {
            value
        } else {
            let value = Arc::new(tree_snapshot(&self.registry, self.pr_cache.clone()).await);
            self.computes.fetch_add(1, Ordering::Relaxed);
            *state = Some(CachedTree {
                generation,
                computed_at: Instant::now(),
                value: Arc::clone(&value),
            });
            value
        };
        // Release the lock before the (deeper) clone of the shared value out.
        drop(state);
        (*value).clone()
    }

    /// How many times the tree was actually built — the coalescing assertion in
    /// tests (N reads within one tick/generation should build once).
    #[cfg(test)]
    fn compute_count(&self) -> u64 {
        self.computes.load(Ordering::Relaxed)
    }
}

/// Builds the `{ repos, show_closed }` snapshot shared by the `tree` op and the
/// `subscribe` stream, so the two never drift (#1301). Two cheap registry locks
/// (the seed folders to derive repos from, and the live windows to join on) and
/// a lock-free read of the toggle, then the git enumeration/enrichment off the
/// lock on a blocking thread inside [`tree_repos`].
async fn tree_snapshot(registry: &WorktreesRegistry, pr_cache: Arc<PrStatusCache>) -> Value {
    let folders = registry.open_folders();
    let windows = registry.list();
    let show_closed = registry.show_closed();
    json!({
        "repos": tree_repos(folders, windows, pr_cache).await,
        "show_closed": show_closed,
    })
}

/// A short human name for a window: its repo, else its first folder's basename,
/// else a placeholder.
fn display_name(entry: &WindowEntry) -> String {
    if let Some(repo) = &entry.repo {
        return repo.clone();
    }
    if let Some(folder) = entry.folders.first() {
        return folder.file_name().map_or_else(
            || folder.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
    }
    "(no folder)".to_string()
}

/// Separator between the repo name and branch for a normal working tree.
const REPO_SEP: char = '·';
/// Separator marking a **linked worktree** (a git "fork" glyph), so a worktree
/// line is distinguishable at a glance from its parent repo's main checkout.
const WORKTREE_SEP: char = '⑂';

/// The full tray item list for a window set: the "No open windows" placeholder
/// when empty, else one line per window via [`window_menu_items`]. Does the git
/// enrichment (blocking disk I/O), so it runs on a blocking thread from the
/// background refresh task — and inline only as a cold-start fallback in `menu`.
fn menu_items_for(entries: &[WindowEntry]) -> Vec<MenuItem> {
    if entries.is_empty() {
        vec![MenuItem::Label("No open windows".to_string())]
    } else {
        window_menu_items(entries)
    }
}

/// Builds the tray items for a non-empty window list: **one clickable line per
/// window** whose label carries the live git state and whose click focuses that
/// window. A window with no workspace folder has nothing for `code` to open, so
/// it stays a non-clickable status line. The labels read each worktree from disk
/// (via [`window_label`]) — cheap for a realistic window count and consistent
/// with reap-on-read.
fn window_menu_items(entries: &[WindowEntry]) -> Vec<MenuItem> {
    entries
        .iter()
        .map(|entry| {
            let label = window_label(entry);
            if entry.folders.is_empty() {
                MenuItem::Label(label)
            } else {
                MenuItem::Action(MenuAction {
                    id: format!("focus:{}", entry.key),
                    label,
                    enabled: true,
                })
            }
        })
        .collect()
}

/// The tray label for one window: the **main repository** name, then live branch
/// state (`omni-dev · branch (+2 -1)`) when the primary folder is a git repo. A
/// linked worktree is set off with the [`WORKTREE_SEP`] fork glyph
/// (`omni-dev ⑂ branch`) so it reads distinctly from the main checkout; a folder
/// that is not a repo falls back to its reported title.
fn window_label(entry: &WindowEntry) -> String {
    let status = entry
        .folders
        .first()
        .map(|folder| git_status(folder))
        .unwrap_or_default();
    // Prefer the git-derived main repo so a linked worktree names its parent
    // repository rather than its worktree-folder basename.
    let name = status
        .main_repo
        .clone()
        .unwrap_or_else(|| display_name(entry));
    if let Some(branch) = &status.branch {
        let sep = if status.is_worktree {
            WORKTREE_SEP
        } else {
            REPO_SEP
        };
        return match sync_indicator(status.ahead, status.behind) {
            Some(sync) => format!("{name} {sep} {branch} {sync}"),
            None => format!("{name} {sep} {branch}"),
        };
    }
    // No git branch (not a repo / detached): fall back to the reported title.
    match &entry.title {
        Some(title) if title != &name => format!("{name} {REPO_SEP} {title}"),
        _ => name,
    }
}

/// A compact `(+ahead -behind)` divergence indicator, or `None` when the branch
/// has no upstream to compare against.
fn sync_indicator(ahead: Option<usize>, behind: Option<usize>) -> Option<String> {
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => Some(format!("(+{ahead} -{behind})")),
        _ => None,
    }
}

/// Well-known absolute locations for the VS Code launcher, tried in order so a
/// daemon running under launchd (with a minimal `PATH`) still finds it.
const CODE_BINARY_CANDIDATES: &[&str] = &[
    "/usr/local/bin/code",
    "/opt/homebrew/bin/code",
    "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
    "/usr/bin/code",
];

/// Focuses (or opens, since VS Code reuses an already-open window) `folder` in
/// VS Code by spawning its CLI, resolved via [`resolve_code_binary`]. Shared
/// with the sessions service's tray "focus" action, which resolves a session to
/// its VS Code window folder and opens it through this same guarded launcher.
pub(crate) fn focus_window(folder: &Path) -> Result<()> {
    focus_window_with(&resolve_code_binary(), folder)
}

/// Spawns `program` on `folder` after validating the folder. Split out from
/// [`focus_window`] so the validation and spawn paths are testable with an
/// explicit launcher (no environment or installed-editor dependency).
///
/// Best-effort and non-blocking: the spawned child is reaped on a detached
/// thread so a long-lived daemon does not accumulate zombies one per focus.
fn focus_window_with(program: &Path, folder: &Path) -> Result<()> {
    // The tray path passes an absolute workspace folder, but the socket `open`
    // op (#1266) passes an arbitrary client-supplied path, so this guard is a
    // real check there, not just an assertion: requiring an absolute path also
    // rules out a `-`-leading path being parsed by `code` as a flag.
    if !folder.is_absolute() {
        bail!(
            "refusing to focus a non-absolute folder path: {}",
            folder.display()
        );
    }
    if !folder.is_dir() {
        bail!("worktree folder no longer exists: {}", folder.display());
    }
    // Detach the launcher's stdio so its output never interleaves into the
    // long-lived daemon's own stdout/stderr (or the test harness's).
    let child = Command::new(program)
        .arg(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to launch `{}` to focus {}",
                program.display(),
                folder.display()
            )
        })?;
    // Reap the child without blocking so it never lingers as a zombie.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Resolves the VS Code launcher from the real environment: the
/// `OMNI_DEV_VSCODE_BIN` override, then [`CODE_BINARY_CANDIDATES`], then bare
/// `code` on `PATH`. The pure resolution logic lives in
/// [`resolve_code_binary_from`] for testing.
fn resolve_code_binary() -> PathBuf {
    resolve_code_binary_from(std::env::var_os(VSCODE_BIN_ENV), CODE_BINARY_CANDIDATES)
}

/// Pure launcher resolution: `env_override` wins; otherwise the first existing
/// `candidate`; otherwise bare `code`.
fn resolve_code_binary_from(
    env_override: Option<std::ffi::OsString>,
    candidates: &[&str],
) -> PathBuf {
    if let Some(path) = env_override {
        return PathBuf::from(path);
    }
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.exists() {
            return path.to_path_buf();
        }
    }
    PathBuf::from("code")
}

// --- Close op (#1277) --------------------------------------------------------

/// The `close` op payload: close a worktree's window and (for a linked worktree)
/// delete it. Symmetric to `open`, but destructive, so it carries the
/// two-phase-confirm and self-close routing fields.
#[derive(Debug, Clone, Deserialize)]
struct CloseRequest {
    /// Absolute path of the target worktree's working directory.
    path: PathBuf,
    /// The requesting window's key, so a self-close (`requester_key` owns the
    /// target) removes-then-replies and lets the extension close its own window,
    /// rather than waiting on a window that is blocked awaiting this reply.
    #[serde(default)]
    requester_key: Option<String>,
    /// Whether to **delete** the worktree (linked "Close Worktree") rather than
    /// only close its window (main "Close Window"). A delete is refused on the
    /// main working tree regardless of this flag.
    #[serde(default)]
    remove: bool,
    /// Set on the phase-2 execute call. Absent/false with `remove:true` is the
    /// phase-1, side-effect-free safety check; ignored for `remove:false`.
    #[serde(default)]
    confirmed: bool,
}

/// One risk or informational note in a [`SafetyReport`]: a machine-readable
/// `kind` and a human-readable `detail`. Shared by both the blocking `risks`
/// (data would be lost) and the non-blocking `info` (context, e.g. unpushed
/// commits that survive because the branch is kept).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Note {
    /// A stable machine slug for the condition (e.g. `dirty`, `untracked`).
    kind: String,
    /// A human-readable one-line explanation for the confirm dialog.
    detail: String,
}

impl Note {
    fn new(kind: &str, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            detail: detail.into(),
        }
    }
}

/// The phase-1 safety report the extension reads to decide whether to prompt.
/// `removable && risks.is_empty()` → proceed with **no** dialog; any `risks`
/// entry → show a modal confirm listing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SafetyReport {
    /// Whether the target is a deletable (linked) worktree at all — `false` for
    /// the main working tree, which the daemon never removes.
    removable: bool,
    /// Whether the target is the repository's main working tree.
    is_main: bool,
    /// Whether a live VS Code window currently has the target open.
    open: bool,
    /// The owning window's key, when `open` (the first, for the wait/close).
    #[serde(skip_serializing_if = "Option::is_none")]
    window_key: Option<String>,
    /// How many workspace folders the owning window has — so the extension can
    /// warn "this window has N folders open; all will close" (failure mode #10).
    window_folder_count: usize,
    /// Conditions that would lose data on removal; a non-empty list forces a
    /// confirm dialog.
    risks: Vec<Note>,
    /// Non-blocking context shown for awareness (e.g. unpushed commits that
    /// survive because the branch is kept).
    info: Vec<Note>,
}

/// The git-only half of the safety check, before the registry's open-window
/// facts are folded in. Pure disk I/O; computed on a blocking thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GitSafety {
    is_main: bool,
    removable: bool,
    risks: Vec<Note>,
    info: Vec<Note>,
}

/// Live windows (key, workspace-folder count) that currently have `path` open,
/// matched by canonicalized path so a symlinked or `..`-laden report still
/// joins. Disk I/O (canonicalization), so it runs on a blocking thread.
fn windows_with_path(entries: &[WindowEntry], path: &Path) -> Vec<(String, usize)> {
    let target = canonical(path);
    entries
        .iter()
        .filter(|e| e.folders.iter().any(|f| canonical(f) == target))
        .map(|e| (e.key.clone(), e.folders.len()))
        .collect()
}

/// How long the execute phase waits for a signalled window to close
/// (`unregister`) before giving up. Deliberately generous against the ~10s
/// heartbeat interval the close directive rides — a window may have just
/// heartbeated, so the directive is only picked up on the *next* one — plus the
/// window's own close/save latency. The keyed-push responsiveness upgrade
/// (#1277 fast-follow) removes this wait entirely.
const CLOSE_WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the execute phase re-checks whether the signalled windows have
/// unregistered.
const CLOSE_WAIT_POLL: Duration = Duration::from_millis(250);

/// Waits up to `timeout` for every window *other than* `requester` that has
/// `path` open to unregister (close), polling the live registry every `poll`.
/// A window whose `last_seen` has already gone stale is reaped by `list()` and
/// so counts as closed. Returns an error naming the still-open windows on
/// timeout, so the caller can surface "window did not close" and leave the
/// worktree untouched (failure modes #4/#5).
async fn await_windows_closed(
    registry: &WorktreesRegistry,
    path: &Path,
    requester: Option<&str>,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // The registry read is cheap CPU, but the path canonicalization in
        // `windows_with_path` is disk I/O — do the whole check on a blocking
        // thread, never on the async worker.
        let entries = registry.list();
        let path = path.to_path_buf();
        let requester = requester.map(str::to_string);
        let remaining: Vec<String> = tokio::task::spawn_blocking(move || {
            windows_with_path(&entries, &path)
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| requester.as_deref() != Some(k))
                .collect()
        })
        .await
        .unwrap_or_default();

        if remaining.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("window(s) did not close in time: {}", remaining.join(", "));
        }
        tokio::time::sleep(poll).await;
    }
}

/// Computes the [`GitSafety`] of a worktree at `path`: whether it is the main
/// working tree (never removable) and, for a linked worktree, what a removal
/// would lose. Best-effort per-check but the overall open must succeed — a path
/// that is not a git worktree is a hard error (we refuse to delete an unknown
/// directory). A path that no longer exists is treated as an already-removed
/// linked worktree so the idempotent execute path can proceed with no dialog.
fn git_safety(path: &Path) -> Result<GitSafety> {
    if !path.exists() {
        return Ok(GitSafety {
            is_main: false,
            removable: true,
            risks: vec![],
            info: vec![Note::new("already-removed", "worktree no longer exists")],
        });
    }
    let repo = Repository::open(path)
        .with_context(|| format!("not a git worktree: {}", path.display()))?;
    // The one structural fact deletability keys off — never the branch name.
    if !repo.is_worktree() {
        return Ok(GitSafety {
            is_main: true,
            removable: false,
            risks: vec![],
            info: vec![Note::new(
                "main-working-tree",
                "the repository's main working tree is never deleted",
            )],
        });
    }

    let mut risks = Vec::new();
    let mut info = Vec::new();

    let (dirty, untracked) = count_dirty_untracked(&repo);
    if dirty > 0 {
        risks.push(Note::new(
            "dirty",
            format!("{dirty} modified tracked file(s) would be lost"),
        ));
    }
    if untracked > 0 {
        risks.push(Note::new(
            "untracked",
            format!("{untracked} untracked file(s) would be lost"),
        ));
    }

    // An in-progress rebase/merge/cherry-pick etc. is lost on removal.
    let state = repo.state();
    if state != RepositoryState::Clean {
        risks.push(Note::new(
            "in-progress",
            format!("an in-progress {state:?} operation would be lost"),
        ));
    }

    // Commits reachable only from a detached HEAD are GC'd once the worktree —
    // and its HEAD ref — are gone. A HEAD still reachable from any ref (a branch
    // or tag) loses nothing, so it is not flagged.
    if repo.head_detached().unwrap_or(false) {
        let lost = unreachable_commit_count(&repo).unwrap_or(0);
        if lost > 0 {
            risks.push(Note::new(
                "unreachable-commits",
                format!("{lost} commit(s) on a detached HEAD will be permanently lost"),
            ));
        }
    }

    // Unpushed commits on a *named* branch survive: removal never deletes the
    // branch. Informational only — it must not block or prompt.
    if let Some(ahead) = current_branch_ahead(&repo) {
        if ahead > 0 {
            info.push(Note::new(
                "unpushed",
                format!("{ahead} unpushed commit(s) on the branch (kept — the branch survives)"),
            ));
        }
    }

    Ok(GitSafety {
        is_main: false,
        removable: true,
        risks,
        info,
    })
}

/// Counts a worktree's `(dirty tracked, untracked)` files. Tracked covers any
/// staged or unstaged modification (including conflicts and deletions);
/// untracked is `WT_NEW`. `.gitignore`d files are excluded — they are
/// regenerable and must not force a prompt — via `include_ignored(false)`, so no
/// status entry ever carries the `IGNORED` bit. A failed status read degrades to
/// `(0, 0)` rather than sinking the whole safety check.
fn count_dirty_untracked(repo: &Repository) -> (usize, usize) {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return (0, 0);
    };
    // Any staged or unstaged change to a tracked path (WT_NEW is untracked, so
    // it is deliberately excluded from this mask).
    let tracked = Status::INDEX_NEW
        | Status::INDEX_MODIFIED
        | Status::INDEX_DELETED
        | Status::INDEX_RENAMED
        | Status::INDEX_TYPECHANGE
        | Status::WT_MODIFIED
        | Status::WT_DELETED
        | Status::WT_TYPECHANGE
        | Status::WT_RENAMED
        | Status::CONFLICTED;
    let mut dirty = 0;
    let mut untracked = 0;
    for entry in statuses.iter() {
        let s = entry.status();
        if s.contains(Status::WT_NEW) {
            untracked += 1;
        }
        if s.intersects(tracked) {
            dirty += 1;
        }
    }
    (dirty, untracked)
}

/// Counts commits reachable from the (detached) HEAD but from no other ref —
/// the commits git would garbage-collect once the worktree's HEAD is gone.
/// `None` if HEAD or the revwalk cannot be resolved. The literal `HEAD` ref is
/// skipped (hiding it would hide the very commits we are counting); every real
/// branch/tag/remote ref is hidden, so a tip that any branch also points at
/// yields `0` (nothing is actually lost).
fn unreachable_commit_count(repo: &Repository) -> Option<usize> {
    let head_oid = repo.head().ok()?.target()?;
    let mut walk = repo.revwalk().ok()?;
    walk.push(head_oid).ok()?;
    for reference in repo.references().ok()? {
        let Ok(reference) = reference else { continue };
        // Skip the literal HEAD ref — hiding it would hide the very commits we
        // are counting; every real branch/tag/remote ref is hidden below.
        if matches!(reference.name(), Ok("HEAD")) {
            continue;
        }
        if let Some(oid) = reference.target() {
            let _ = walk.hide(oid);
        }
    }
    Some(walk.flatten().count())
}

/// Commits the worktree's current branch is ahead of its upstream, or `None`
/// when HEAD is detached or the branch tracks no upstream. Reuses
/// [`upstream_ahead_behind`]; only the ahead count matters here (unpushed work).
fn current_branch_ahead(repo: &Repository) -> Option<usize> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    let branch = git2::Branch::wrap(head);
    upstream_ahead_behind(repo, &branch).map(|(ahead, _behind)| ahead)
}

/// Resolves the linked worktree whose working directory canonicalizes to
/// `target` to its registered name in `main_repo`. Errors when `target` is not
/// one of the repo's worktrees — the defensive guard against removing a path
/// that opened as a worktree but is not enumerated. Split out so that guard is
/// unit-testable without corrupting git's worktree admin state.
fn worktree_name_for_path(main_repo: &Repository, target: &Path) -> Result<String> {
    let names = main_repo.worktrees()?;
    names
        .iter()
        .flatten() // Result<Option<&str>, _> → Option<&str> (drop per-name errors)
        .flatten() // Option<&str> → &str (drop non-UTF-8 names)
        .find(|name| {
            main_repo
                .find_worktree(name)
                .is_ok_and(|wt| canonical(wt.path()) == target)
        })
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "worktree {} is not registered in {}",
                target.display(),
                main_repo.path().display()
            )
        })
}

/// Backoff delays between recursive-removal retries (#1315). A concurrent
/// writer — a just-closed window's language server (Metals/Bloop) or
/// `rust-analyzer`/`cargo` still flushing build artifacts into `target/` — can
/// create a file between our directory scan and its `rmdir`, making the removal
/// fail with `ENOTEMPTY` ("Directory not empty"). Each retry re-sweeps and
/// waits longer, giving the winding-down process time to quiesce. Total wait
/// ~2.75s across four retries; the window teardown the caller already waited on
/// dominates it.
const WORKTREE_RMDIR_BACKOFF: &[Duration] = &[
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(1),
];

/// Whether `e` is the transient "directory re-populated under us" race we retry
/// (see [`WORKTREE_RMDIR_BACKOFF`]) rather than a hard failure (permission
/// denied, read-only filesystem) we must surface immediately. Matches the raw
/// errno — `std::io::ErrorKind::DirectoryNotEmpty` is only stable from Rust 1.83,
/// past our MSRV — including the `EEXIST`/`EBUSY` siblings libgit2 lumps in.
fn is_transient_rmdir_error(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(nix::libc::ENOTEMPTY | nix::libc::EEXIST | nix::libc::EBUSY)
    )
}

/// Recursively removes `dir`, retrying on the transient concurrent-writer race
/// (see [`is_transient_rmdir_error`]) and treating an already-absent directory
/// as success. Non-transient errors surface immediately with the original
/// message. Runs on a blocking thread (called only from [`remove_worktree`], via
/// `spawn_blocking`), so the between-retry `sleep` is fine.
fn remove_dir_all_retrying(dir: &Path) -> Result<()> {
    remove_dir_all_retrying_with(dir, WORKTREE_RMDIR_BACKOFF, || std::fs::remove_dir_all(dir))
}

/// [`remove_dir_all_retrying`] with the schedule and the removal itself injected.
/// Provoking the real race requires a concurrent writer to lose a timing window,
/// so only an injected sequence of errors can drive every branch of the loop —
/// exhausting the backoff especially — deterministically and without sleeping out
/// the production schedule.
fn remove_dir_all_retrying_with(
    dir: &Path,
    backoff: &[Duration],
    mut remove: impl FnMut() -> std::io::Result<()>,
) -> Result<()> {
    let mut backoff = backoff.iter();
    loop {
        match remove() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                if is_transient_rmdir_error(&e) {
                    if let Some(delay) = backoff.next() {
                        std::thread::sleep(*delay);
                        continue;
                    }
                }
                return Err(e).with_context(|| {
                    format!("failed to remove worktree directory {}", dir.display())
                });
            }
        }
    }
}

/// Whether `path` is a **half-removed** linked worktree: its `.git` gitlink
/// still points at an admin directory a prior failed removal already deleted.
/// libgit2's combined prune deletes the admin metadata *before* it rmdirs the
/// working tree, so a working-tree rmdir failure (#1315) leaves exactly this
/// orphan — the directory on disk with a dangling gitlink, no longer tracked by
/// git. Safe to delete outright: a live worktree's gitlink resolves (its repo
/// opens) and a normal checkout has a `.git` *directory*, so this matches
/// neither.
fn is_orphaned_worktree(path: &Path) -> bool {
    // `read_to_string` fails on a `.git` directory (a normal checkout), so only
    // a linked worktree's gitlink file gets past here.
    let Ok(contents) = std::fs::read_to_string(path.join(".git")) else {
        return false;
    };
    let Some(admin) = contents.strip_prefix("gitdir:").map(str::trim) else {
        return false;
    };
    let admin = Path::new(admin);
    // A linked-worktree admin path (`…/worktrees/<name>`) whose target is gone.
    admin.components().any(|c| c.as_os_str() == "worktrees") && !admin.exists()
}

/// Removes a **linked** worktree at `path` via `git2` (no shell — avoiding the
/// daemon-`PATH` problem the launcher fights): deletes both the checked-out
/// directory and the admin metadata. Refuses the main working tree (the
/// defensive backstop behind the UI gating) and a locked worktree (surfacing
/// "unlock first" rather than forcing past the lock). Idempotent: an
/// already-removed path is a success.
///
/// The working tree is removed **first** (retrying to absorb the
/// concurrent-writer race, #1315), and only then is the admin metadata pruned.
/// This is deliberately the reverse of libgit2's combined
/// `prune(working_tree: true)`, which deletes the admin dir first and, when the
/// working-tree rmdir then fails, leaves a **half-removed orphan** git no longer
/// tracks (and which a naive prune-retry cannot recover, since its admin gitdir
/// is already gone). Doing the directory first means a transient failure leaves
/// the worktree fully tracked and cleanly retryable; a pre-existing orphan from
/// the old ordering is detected and its leftover directory cleaned up directly.
fn remove_worktree(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let repo = match Repository::open(path) {
        Ok(repo) => repo,
        // Admin metadata already gone (a prior failed removal); git no longer
        // tracks this path, so no prune applies — just delete the leftover.
        Err(_) if is_orphaned_worktree(path) => return remove_dir_all_retrying(path),
        Err(e) => return Err(e).context(format!("not a git worktree: {}", path.display())),
    };
    if !repo.is_worktree() {
        bail!(
            "refusing to delete the main working tree: {}",
            path.display()
        );
    }
    // The Worktree handle lives on the *main* repo (the common dir's parent),
    // keyed by name; find it by matching the target path.
    let commondir = canonical(repo.commondir());
    let main_root = commondir
        .parent()
        .ok_or_else(|| anyhow!("no repository root for {}", path.display()))?
        .to_path_buf();
    // Drop the worktree-scoped handle before we delete its directory.
    drop(repo);
    let main_repo = Repository::open(&main_root)
        .with_context(|| format!("failed to open repository at {}", main_root.display()))?;
    let name = worktree_name_for_path(&main_repo, &canonical(path))?;
    let worktree = main_repo.find_worktree(&name)?;

    // Never silently force past a lock (failure mode #6).
    if let WorktreeLockStatus::Locked(reason) = worktree.is_locked()? {
        let because = reason.map(|r| format!(" ({r})")).unwrap_or_default();
        bail!("worktree is locked{because}; unlock it first (git worktree unlock)");
    }

    // Delete the checked-out directory ourselves, retrying past the
    // concurrent-writer race (#1315).
    remove_dir_all_retrying(path)?;

    // The directory is gone; prune only the admin metadata. working_tree(false)
    // keeps git2 from re-attempting (and failing on) the now-absent directory;
    // valid(true) prunes even though the worktree was valid; locked stays false,
    // so a lock (re-checked above) is never forced.
    let mut opts = git2::WorktreePruneOptions::new();
    opts.valid(true).working_tree(false);
    worktree
        .prune(Some(&mut opts))
        .with_context(|| format!("failed to prune worktree metadata for {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::shim::{shim_lock, write_exec_script};
    use chrono::Utc;
    use std::sync::MutexGuard;

    fn register_payload(key: &str, repo: Option<&str>, folder: &str) -> Value {
        json!({
            "key": key,
            "folders": [folder],
            "repo": repo,
            "title": format!("{key}-title"),
            "pid": 1234,
        })
    }

    /// Pulls the `windows` array out of a `list`/`status` payload.
    fn windows_of(payload: &Value) -> &Vec<Value> {
        payload
            .get("windows")
            .and_then(Value::as_array)
            .expect("windows array")
    }

    #[tokio::test]
    async fn name_and_unknown_op() {
        let svc = WorktreesService::new();
        assert_eq!(svc.name(), "worktrees");
        assert!(svc.handle("frobnicate", Value::Null).await.is_err());
    }

    #[tokio::test]
    async fn handle_routes_ops_and_shapes_payloads() {
        let svc = WorktreesService::new();
        // Empty to start.
        let payload = svc.handle("list", Value::Null).await.unwrap();
        assert_eq!(payload, json!({ "windows": [] }));

        // register → { ok: true }, then it shows up in list.
        let reply = svc
            .handle("register", register_payload("w1", Some("repo-a"), "/tmp/a"))
            .await
            .unwrap();
        assert_eq!(reply, json!({ "ok": true }));
        let windows = windows_of(&svc.handle("list", Value::Null).await.unwrap()).clone();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].get("key").and_then(Value::as_str), Some("w1"));
        assert!(windows[0].get("last_seen").is_some());

        // heartbeat known/unknown.
        let known = svc
            .handle("heartbeat", json!({ "key": "w1" }))
            .await
            .unwrap();
        assert_eq!(known, json!({ "known": true }));
        let unknown = svc
            .handle("heartbeat", json!({ "key": "nope" }))
            .await
            .unwrap();
        assert_eq!(unknown, json!({ "known": false }));

        // unregister removes, then repeats as a no-op success.
        let gone = svc
            .handle("unregister", json!({ "key": "w1" }))
            .await
            .unwrap();
        assert_eq!(gone, json!({ "removed": true }));
        let again = svc
            .handle("unregister", json!({ "key": "w1" }))
            .await
            .unwrap();
        assert_eq!(again, json!({ "removed": false }));
    }

    #[tokio::test]
    async fn handle_rejects_missing_or_empty_key() {
        let svc = WorktreesService::new();
        // register validates a present, non-blank key.
        assert!(svc.handle("register", json!({})).await.is_err());
        assert!(svc
            .handle("register", json!({ "key": "  " }))
            .await
            .is_err());
        // heartbeat/unregister require the key via `require_str`.
        assert!(svc.handle("heartbeat", json!({})).await.is_err());
        assert!(svc.handle("unregister", json!({})).await.is_err());
    }

    #[test]
    fn display_name_prefers_repo_then_folder_basename() {
        let base = WindowEntry {
            key: "k".to_string(),
            folders: vec![PathBuf::from("/home/me/project")],
            repo: Some("my-repo".to_string()),
            title: None,
            pid: None,
            last_seen: Utc::now(),
        };
        assert_eq!(display_name(&base), "my-repo");

        let no_repo = WindowEntry {
            repo: None,
            ..base.clone()
        };
        assert_eq!(display_name(&no_repo), "project");

        let nothing = WindowEntry {
            repo: None,
            folders: vec![],
            ..base.clone()
        };
        assert_eq!(display_name(&nothing), "(no folder)");

        // A folder with no basename (the filesystem root) falls back to its
        // displayed path rather than panicking or yielding an empty name.
        let rootish = WindowEntry {
            repo: None,
            folders: vec![PathBuf::from("/")],
            ..base
        };
        assert_eq!(display_name(&rootish), "/");
    }

    #[test]
    fn window_menu_items_merge_stats_and_focus_into_one_clickable_line() {
        let now = Utc::now();
        let entries = vec![
            // A folderless window has nothing to focus, so it stays a plain
            // Label; a title equal to the name collapses to just the name. It
            // leads the list so the focus-action lookup below is exercised
            // against a leading non-Action item it has to skip.
            WindowEntry {
                key: "k2".to_string(),
                folders: vec![],
                repo: Some("solo".to_string()),
                title: Some("solo".to_string()),
                pid: None,
                last_seen: now,
            },
            // A folder-bearing, non-repo window: one clickable Action whose label
            // is the stats line ("name · title", since /tmp is not a git repo).
            WindowEntry {
                key: "k1".to_string(),
                folders: vec![PathBuf::from("/tmp/a")],
                repo: Some("repo".to_string()),
                title: Some("a branch".to_string()),
                pid: None,
                last_seen: now,
            },
        ];
        let items = window_menu_items(&entries);
        // Exactly one item per window — no duplicate label, no separator.
        assert_eq!(items.len(), 2);
        assert!(!items.iter().any(|i| matches!(i, MenuItem::Separator)));

        // The folder-bearing window is a single clickable action carrying the
        // stats label (the old label + Focus action, merged).
        let action = items
            .iter()
            .find_map(|i| match i {
                MenuItem::Action(a) => Some(a),
                _ => None,
            })
            .expect("a focus action");
        assert_eq!(action.id, "focus:k1");
        assert_eq!(action.label, "repo · a branch");

        // The folderless window is a non-clickable label (not "solo · solo").
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Label(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["solo"]);
    }

    #[tokio::test]
    async fn menu_and_status_shapes() {
        let svc = WorktreesService::new();
        // Empty.
        let menu = svc.menu();
        assert_eq!(menu.title, "Worktrees");
        assert!(matches!(
            menu.items.first(),
            Some(MenuItem::Label(text)) if text == "No open windows"
        ));
        let status = svc.status().await;
        assert_eq!(status.name, "worktrees");
        assert!(status.healthy);
        assert_eq!(status.summary, "0 window(s) across 0 repo(s)");

        // Two folder-bearing windows in the same repo, plus one folderless
        // window that shares the repo but has nothing for `code` to open.
        svc.handle("register", register_payload("w1", Some("repo-a"), "/tmp/a"))
            .await
            .unwrap();
        svc.handle("register", register_payload("w2", Some("repo-a"), "/tmp/b"))
            .await
            .unwrap();
        svc.handle(
            "register",
            json!({ "key": "w3", "repo": "repo-a", "folders": [] }),
        )
        .await
        .unwrap();
        let status = svc.status().await;
        assert_eq!(status.summary, "3 window(s) across 1 repo(s)");

        let menu = svc.menu();
        // One line per window — no separator, no duplicate label.
        assert_eq!(menu.items.len(), 3);
        assert!(!menu.items.iter().any(|i| matches!(i, MenuItem::Separator)));
        let action_ids: Vec<&str> = menu
            .items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Action(a) => Some(a.id.as_str()),
                _ => None,
            })
            .collect();
        // The two folder-bearing windows are clickable; the folderless one is a
        // plain Label, so it never yields a focus action.
        assert!(action_ids.contains(&"focus:w1"));
        assert!(action_ids.contains(&"focus:w2"));
        assert!(!action_ids.contains(&"focus:w3"));
    }

    #[test]
    fn start_menu_refresh_is_a_noop_outside_a_runtime() {
        // With no tokio runtime, the background task is never spawned, so the
        // bare service keeps computing `menu()` inline (what the tests rely on).
        let svc = WorktreesService::new();
        svc.start_menu_refresh();
        assert!(svc.refresh.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn start_menu_refresh_populates_cache_and_shutdown_stops_it() {
        let svc = WorktreesService::new();
        svc.handle("register", register_payload("w1", Some("repo-a"), "/tmp/a"))
            .await
            .unwrap();
        // Before the task runs, `menu()` computes inline from an empty cache.
        assert!(svc.menu_cache.lock().unwrap().is_none());

        svc.start_menu_refresh();
        // Idempotent: a second call does not start a second task.
        svc.start_menu_refresh();

        // The task fills the cache off the main thread; poll briefly for it.
        let mut filled = false;
        for _ in 0..100 {
            if svc.menu_cache.lock().unwrap().is_some() {
                filled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(filled, "background refresh should populate the menu cache");

        // `menu()` now serves the cache: one clickable line for the window.
        let menu = svc.menu();
        assert_eq!(menu.title, "Worktrees");
        assert!(menu
            .items
            .iter()
            .any(|i| matches!(i, MenuItem::Action(a) if a.id == "focus:w1")));

        // Shutdown cancels and joins the task, clearing the handle.
        svc.shutdown().await;
        assert!(svc.refresh.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn default_constructs_an_empty_service() {
        let svc = WorktreesService::default();
        let payload = svc.handle("list", Value::Null).await.unwrap();
        assert_eq!(payload, json!({ "windows": [] }));
    }

    // --- Push subscription (#1267) -----------------------------------------

    #[tokio::test]
    async fn subscribe_streams_only_for_the_subscribe_op() {
        let svc = WorktreesService::new();
        // The one streaming op yields a stream; every other op (including the
        // request/reply worktrees ops) declines, so the server dispatches them
        // normally.
        assert!(svc.subscribe("subscribe", &Value::Null).is_some());
        assert!(svc.subscribe("list", &Value::Null).is_none());
        assert!(svc.subscribe("register", &Value::Null).is_none());
        assert!(svc.subscribe("bogus", &Value::Null).is_none());
    }

    #[tokio::test]
    async fn subscribe_snapshot_matches_the_tree_op() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();

        let svc = WorktreesService::new();
        let stream = svc
            .subscribe("subscribe", &Value::Null)
            .expect("subscribe stream");
        // No windows yet → no repos derived; the toggle rides along at its
        // default (show all).
        assert_eq!(
            stream.snapshot().await,
            json!({ "repos": [], "show_closed": true })
        );

        // A window opens on the repo → the snapshot carries it, byte-identical to
        // what the `tree` op returns for the same registry state.
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [dir.path()], "repo": "r" }),
        )
        .await
        .unwrap();
        let snap = stream.snapshot().await;
        let tree = svc.handle("tree", Value::Null).await.unwrap();
        assert_eq!(snap, tree);
        let repos = snap["repos"].as_array().expect("repos array");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["worktrees"][0]["branch"], json!("main"));
    }

    #[tokio::test]
    async fn subscribe_changed_wakes_on_register() {
        let svc = WorktreesService::new();
        let mut stream = svc
            .subscribe("subscribe", &Value::Null)
            .expect("subscribe stream");
        // Idle: `changed()` must not resolve without a registry change.
        tokio::select! {
            () = stream.changed() => panic!("changed resolved with no registry change"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        // A register bumps the change-notify → `changed()` resolves promptly.
        svc.handle("register", register_payload("w1", Some("r"), "/tmp/a"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), stream.changed())
            .await
            .expect("changed should resolve after a register");
    }

    // --- Coalesced tree-snapshot cache (#1303) -----------------------------

    #[tokio::test]
    async fn tree_cache_coalesces_reads_within_ttl_and_generation() {
        let reg = Arc::new(WorktreesRegistry::new());
        // A long TTL so only the generation gate is exercised here.
        let cache = TreeSnapshotCache::with_ttl(
            reg,
            Arc::new(PrStatusCache::new()),
            Duration::from_secs(60),
        );
        // The first read builds once.
        let first = cache.snapshot().await;
        assert_eq!(cache.compute_count(), 1);
        // Further reads with no registry change and within the TTL reuse the
        // cached value — no extra build, byte-identical result.
        let second = cache.snapshot().await;
        assert_eq!(
            cache.compute_count(),
            1,
            "an unchanged read must not rebuild"
        );
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn tree_cache_single_flights_a_read_burst() {
        let reg = Arc::new(WorktreesRegistry::new());
        let cache = Arc::new(TreeSnapshotCache::with_ttl(
            reg,
            Arc::new(PrStatusCache::new()),
            Duration::from_secs(60),
        ));
        // A burst of concurrent readers — as N subscriber streams would wake
        // together on a change/tick — collapses to exactly one build; the rest
        // read the shared result (the acceptance criterion).
        let mut handles = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            handles.push(tokio::spawn(async move { cache.snapshot().await }));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }
        assert_eq!(
            cache.compute_count(),
            1,
            "a concurrent read burst must build the tree once"
        );
        assert!(
            results.windows(2).all(|w| w[0] == w[1]),
            "every reader must observe the identical snapshot"
        );
    }

    #[tokio::test]
    async fn tree_cache_rebuilds_on_registry_change() {
        let reg = Arc::new(WorktreesRegistry::new());
        let cache = TreeSnapshotCache::with_ttl(
            reg.clone(),
            Arc::new(PrStatusCache::new()),
            Duration::from_secs(60),
        );
        cache.snapshot().await;
        assert_eq!(cache.compute_count(), 1);
        // A registry change bumps the generation, so the next read rebuilds even
        // though the (long) TTL has not expired — subscribers never see a stale
        // visible set.
        assert!(reg.set_show_closed(false));
        cache.snapshot().await;
        assert_eq!(
            cache.compute_count(),
            2,
            "a generation bump must force a rebuild"
        );
    }

    #[tokio::test]
    async fn tree_cache_rebuilds_after_ttl_expiry() {
        let reg = Arc::new(WorktreesRegistry::new());
        // A zero TTL: every read is already past it, so a pure on-disk git change
        // still surfaces on the next tick with no registry bump needed.
        let cache =
            TreeSnapshotCache::with_ttl(reg, Arc::new(PrStatusCache::new()), Duration::ZERO);
        cache.snapshot().await;
        cache.snapshot().await;
        assert_eq!(
            cache.compute_count(),
            2,
            "an expired TTL must force a rebuild each read"
        );
    }

    #[tokio::test]
    async fn subscribe_streams_share_one_build_per_generation() {
        let svc = WorktreesService::new();
        let s1 = svc
            .subscribe("subscribe", &Value::Null)
            .expect("subscribe stream");
        let s2 = svc
            .subscribe("subscribe", &Value::Null)
            .expect("subscribe stream");
        // Two windows' streams sampling the same registry state build the tree
        // once, not once per stream (#1303) — they share the service's cache.
        let a = s1.snapshot().await;
        let b = s2.snapshot().await;
        assert_eq!(a, b);
        assert_eq!(
            svc.tree_cache.compute_count(),
            1,
            "N streams on one generation must share a single build"
        );
    }

    // --- Show/hide-closed toggle (#1301) -----------------------------------

    #[tokio::test]
    async fn set_show_closed_toggles_the_snapshot_field() {
        let svc = WorktreesService::new();
        // The snapshot carries the toggle; it defaults to show-all.
        assert_eq!(
            svc.handle("tree", Value::Null).await.unwrap()["show_closed"],
            json!(true)
        );
        // Setting it flips the field the next snapshot reports.
        let reply = svc
            .handle("set-show-closed", json!({ "show_closed": false }))
            .await
            .unwrap();
        assert_eq!(reply, json!({ "ok": true }));
        assert_eq!(
            svc.handle("tree", Value::Null).await.unwrap()["show_closed"],
            json!(false)
        );
    }

    #[tokio::test]
    async fn set_show_closed_rejects_a_non_boolean_payload() {
        let svc = WorktreesService::new();
        assert!(svc.handle("set-show-closed", json!({})).await.is_err());
        assert!(svc
            .handle("set-show-closed", json!({ "show_closed": "yes" }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn set_show_closed_wakes_the_subscription() {
        let svc = WorktreesService::new();
        let mut stream = svc
            .subscribe("subscribe", &Value::Null)
            .expect("subscribe stream");
        // A real flip bumps the change-notify → `changed()` resolves promptly.
        svc.handle("set-show-closed", json!({ "show_closed": false }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), stream.changed())
            .await
            .expect("changed should resolve after a toggle flip");
        // The pushed snapshot now reflects the new toggle.
        assert_eq!(stream.snapshot().await["show_closed"], json!(false));
    }

    #[tokio::test]
    async fn menu_action_rejects_unknown_and_missing_window() {
        let svc = WorktreesService::new();
        assert!(svc.menu_action("bogus").await.is_err());
        // A focus for a key with no registration errors rather than spawning.
        assert!(svc.menu_action("focus:nope").await.is_err());
        svc.shutdown().await;
    }

    /// Restores `OMNI_DEV_VSCODE_BIN` on drop. The two spawn tests that read the
    /// variable (via `resolve_code_binary` → `focus_window`) —
    /// `menu_action_focus_resolves_folder_and_spawns` and
    /// `open_focuses_an_existing_absolute_dir` — both point the launcher at the
    /// same harmless `/bin/sh`, and no test asserts the variable is *unset*, so a
    /// transient overlap under the harness's test parallelism is benign.
    struct VscodeBinGuard(Option<std::ffi::OsString>);
    impl Drop for VscodeBinGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var(VSCODE_BIN_ENV, v),
                None => std::env::remove_var(VSCODE_BIN_ENV),
            }
        }
    }

    #[tokio::test]
    async fn menu_action_focus_resolves_folder_and_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [dir.path()], "repo": "r" }),
        )
        .await
        .unwrap();

        // Point the launcher at a harmless binary so the spawn deterministically
        // succeeds and the focus path returns Ok.
        let _g = VscodeBinGuard(std::env::var_os(VSCODE_BIN_ENV));
        std::env::set_var(VSCODE_BIN_ENV, "/bin/sh");
        svc.menu_action("focus:w1").await.unwrap();
    }

    #[tokio::test]
    async fn open_rejects_missing_relative_or_nonexistent_path() {
        let svc = WorktreesService::new();
        // A missing `path` is a payload error.
        assert!(svc.handle("open", json!({})).await.is_err());
        assert!(svc.handle("open", json!({ "path": 42 })).await.is_err());
        // A relative path is rejected before any spawn — this is also what
        // blocks a `-`-leading argument from reaching `code` as a flag.
        assert!(svc
            .handle("open", json!({ "path": "relative/dir" }))
            .await
            .is_err());
        assert!(svc
            .handle("open", json!({ "path": "-flag" }))
            .await
            .is_err());
        // An absolute path that does not exist is rejected before any spawn, so
        // no launcher is needed for these guard cases.
        assert!(svc
            .handle("open", json!({ "path": "/no/such/abs/dir/xyzzy" }))
            .await
            .is_err());
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn open_focuses_an_existing_absolute_dir() {
        let dir = tempfile::tempdir().unwrap();
        let svc = WorktreesService::new();
        // Pin the launcher to a harmless binary so the spawn deterministically
        // succeeds whether or not `code` is installed. Unlike the tray `focus`
        // path, `open` takes the folder straight from the payload — no prior
        // `register` is required.
        let _g = VscodeBinGuard(std::env::var_os(VSCODE_BIN_ENV));
        std::env::set_var(VSCODE_BIN_ENV, "/bin/sh");
        let reply = svc
            .handle("open", json!({ "path": dir.path() }))
            .await
            .unwrap();
        assert_eq!(reply, json!({ "ok": true }));
        svc.shutdown().await;
    }

    #[test]
    fn focus_window_with_validates_folder_then_spawns() {
        let dir = tempfile::tempdir().unwrap();
        // Non-absolute and missing-directory folders are rejected before spawn.
        assert!(focus_window_with(Path::new("/bin/sh"), Path::new("relative/dir")).is_err());
        assert!(
            focus_window_with(Path::new("/bin/sh"), Path::new("/no/such/abs/dir/xyzzy")).is_err()
        );
        // A valid absolute directory spawns the launcher successfully.
        focus_window_with(Path::new("/bin/sh"), dir.path()).unwrap();
        // A missing launcher surfaces the spawn error (with context), not Ok.
        assert!(focus_window_with(Path::new("/no/such/launcher/xyzzy"), dir.path()).is_err());
    }

    #[test]
    fn resolve_code_binary_from_prefers_env_then_candidate_then_fallback() {
        // Env override wins outright.
        assert_eq!(
            resolve_code_binary_from(Some("/custom/code".into()), &["/usr/bin/code"]),
            PathBuf::from("/custom/code")
        );
        // No override: the first existing candidate is chosen.
        let existing = tempfile::NamedTempFile::new().unwrap();
        let existing_path = existing.path().to_str().unwrap();
        assert_eq!(
            resolve_code_binary_from(None, &["/no/such/candidate/xyzzy", existing_path]),
            PathBuf::from(existing_path)
        );
        // Nothing exists: fall back to bare `code` on PATH.
        assert_eq!(
            resolve_code_binary_from(None, &["/no/such/candidate/xyzzy"]),
            PathBuf::from("code")
        );
        // The real-env wrapper resolves without panicking.
        let _ = resolve_code_binary();
    }

    // --- Git enrichment (#1186) --------------------------------------------

    /// Initializes a fresh repo with a deterministic identity so `commit()`
    /// works without depending on a global git config.
    fn init_repo(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        repo
    }

    /// Writes an empty-tree commit (file content is irrelevant to ahead/behind),
    /// optionally moving `refname` to it, and returns its oid.
    fn empty_commit(
        repo: &Repository,
        refname: Option<&str>,
        parents: &[&git2::Commit<'_>],
        msg: &str,
    ) -> git2::Oid {
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(refname, &sig, &sig, msg, &tree, parents)
            .unwrap()
    }

    /// Commits `content` as file `name` onto `refname`, chaining off its current
    /// tip (if any). Unlike [`empty_commit`], the tree carries a real blob, so
    /// the file is checked out into a worktree and can then be modified to
    /// produce a dirty (tracked) status.
    fn commit_file(
        repo: &Repository,
        refname: &str,
        name: &str,
        content: &[u8],
        msg: &str,
    ) -> git2::Oid {
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let blob = repo.blob(content).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert(name, blob, 0o100_644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let parent = repo
            .refname_to_id(refname)
            .ok()
            .and_then(|oid| repo.find_commit(oid).ok());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        repo.commit(Some(refname), &sig, &sig, msg, &tree, &parents)
            .unwrap()
    }

    /// Builds a repo whose `main` is 1 commit ahead of and 1 behind a configured
    /// `origin/main` upstream, so enrichment reports `ahead: 1, behind: 1`.
    fn diverging_repo(dir: &Path) -> Repository {
        let repo = init_repo(dir);
        // A: the shared base on `main`.
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        let a_commit = repo.find_commit(a).unwrap();
        // origin/main diverges to C, a sibling of the local tip.
        let c = empty_commit(&repo, None, &[&a_commit], "C");
        repo.reference("refs/remotes/origin/main", c, true, "origin main")
            .unwrap();
        // Local `main` advances to B → 1 ahead of / 1 behind origin/main.
        empty_commit(&repo, Some("refs/heads/main"), &[&a_commit], "B");
        // Release the commit's borrow of `repo` so it can be returned.
        drop(a_commit);
        repo.set_head("refs/heads/main").unwrap();
        // Configure the tracking relationship so `upstream()` resolves.
        let mut cfg = repo.config().unwrap();
        cfg.set_str("remote.origin.url", "https://example.invalid/x.git")
            .unwrap();
        cfg.set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
            .unwrap();
        cfg.set_str("branch.main.remote", "origin").unwrap();
        cfg.set_str("branch.main.merge", "refs/heads/main").unwrap();
        repo
    }

    #[test]
    fn git_status_reads_branch_and_ahead_behind() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());
        let status = git_status(dir.path());
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.ahead, Some(1));
        assert_eq!(status.behind, Some(1));
        // A normal checkout names itself and is not flagged a worktree.
        assert_eq!(
            status.main_repo.as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );
        assert!(!status.is_worktree);
    }

    #[test]
    fn git_status_empty_repo_is_unborn() {
        // A repo with no commits has an unborn HEAD, so `head()` errors and the
        // branch/sync fields stay empty rather than panicking — but the repo
        // identity is still resolved from the common dir.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let status = git_status(dir.path());
        assert_eq!(status.branch, None);
        // An unborn HEAD has no commit to name, so the SHA is absent too (#1337).
        assert_eq!(status.head_sha, None);
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        assert_eq!(
            status.main_repo.as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );
        assert!(!status.is_worktree);
    }

    #[test]
    fn git_status_no_upstream_reports_branch_only() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let status = git_status(dir.path());
        assert_eq!(status.branch.as_deref(), Some("main"));
        // No upstream → ahead/behind stay absent rather than zero.
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        // …and so does the upstream SHA, so such a branch still renders with no
        // sync indicator at all (#1344).
        assert_eq!(status.upstream_sha, None);
    }

    #[test]
    fn git_status_non_repo_is_empty_detached_reports_repo_without_branch() {
        // A plain directory that is not a git repo yields nothing at all.
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(git_status(plain.path()), GitStatus::default());

        // A detached HEAD reports no branch (and thus no sync), but the repo
        // identity is still resolved from the common dir.
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head_detached(a).unwrap();
        let status = git_status(dir.path());
        assert_eq!(status.branch, None);
        // A detached HEAD has no branch but *does* have a commit — the SHA is
        // resolved before the branch filter, so it survives here (#1337).
        assert_eq!(status.head_sha.as_deref(), Some(a.to_string().as_str()));
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        // A detached HEAD has no branch, so there is no upstream to resolve
        // either — the branch filter returns before the wrap (#1344).
        assert_eq!(status.upstream_sha, None);
        assert_eq!(
            status.main_repo.as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );
        assert!(!status.is_worktree);
    }

    // --- Lazy ahead/behind (#1306) -----------------------------------------

    #[test]
    fn git_status_cheap_reads_branch_but_skips_the_divergence_walk() {
        // The same repo `git_status` reports 1/1 for. The cheap variant used by
        // the streamed tree snapshot still reads the branch and repo identity, but
        // leaves ahead/behind absent — divergence is now lazy (#1306).
        let dir = tempfile::tempdir().unwrap();
        let repo = diverging_repo(dir.path());
        let status = git_status_cheap(dir.path());
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        assert_eq!(
            status.main_repo.as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );
        // The SHA rides the *cheap* path deliberately: it is a refs read, not a
        // revwalk, and it is what makes a new commit a snapshot delta (#1337).
        let head = repo.head().unwrap().target().unwrap();
        assert_eq!(status.head_sha.as_deref(), Some(head.to_string().as_str()));
    }

    // --- HEAD SHA on the snapshot (#1337) ----------------------------------

    #[test]
    fn git_status_head_sha_tracks_new_commits() {
        // The regression #1337 turns on: a commit must change the status the
        // snapshot is built from. Before the SHA rode the payload, committing
        // changed nothing on the wire, the server's diff dropped the identical
        // snapshot, and no client re-rendered — so a badge computed for the old
        // head survived the push that invalidated it.
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let before = git_status_cheap(dir.path());
        assert_eq!(before.head_sha.as_deref(), Some(a.to_string().as_str()));

        let head = repo.find_commit(a).unwrap();
        let b = empty_commit(&repo, Some("refs/heads/main"), &[&head], "B");
        let after = git_status_cheap(dir.path());
        assert_eq!(after.head_sha.as_deref(), Some(b.to_string().as_str()));
        assert_ne!(before.head_sha, after.head_sha);
        // The branch is unchanged — the SHA is the *only* thing that moved, which
        // is exactly why its absence made the push invisible.
        assert_eq!(before.branch, after.branch);
    }

    // --- Upstream SHA on the snapshot (#1344) ------------------------------

    /// Repoints `refs/remotes/origin/main` at `oid` — exactly what a `git push`
    /// does, and all of what it does: the local branch and HEAD do not move. Lets
    /// these tests exercise a push with no network and no second repo.
    fn simulate_push(repo: &Repository, oid: git2::Oid) {
        repo.reference("refs/remotes/origin/main", oid, true, "push")
            .unwrap();
    }

    #[test]
    fn git_status_upstream_sha_tracks_a_push() {
        // The regression #1344 turns on. `diverging_repo` leaves local `main` at B
        // and origin/main at C — 1 ahead, 1 behind. Pushing B moves *only* the
        // remote-tracking ref, so before this field rode the payload every wire
        // field was byte-identical across the push, the server's diff dropped the
        // snapshot, no client re-rendered, and the lazily-fetched ahead/behind was
        // never re-asked — the row showed `↑1 ↓0` forever.
        let dir = tempfile::tempdir().unwrap();
        let repo = diverging_repo(dir.path());
        let before = git_status(dir.path());
        assert_eq!(before.ahead, Some(1));
        assert_eq!(before.behind, Some(1));

        let head = repo.head().unwrap().target().unwrap();
        simulate_push(&repo, head);
        let after = git_status(dir.path());

        // The upstream now names the pushed commit, and the counts agree.
        assert_eq!(
            after.upstream_sha.as_deref(),
            Some(head.to_string().as_str())
        );
        assert_ne!(before.upstream_sha, after.upstream_sha);
        assert_eq!(after.ahead, Some(0));
        assert_eq!(after.behind, Some(0));
        // Nothing else moved — which is the whole point. A push leaves the branch
        // and the local head exactly where they were, so `upstream_sha` is the
        // only signal a client could possibly notice.
        assert_eq!(before.branch, after.branch);
        assert_eq!(before.head_sha, after.head_sha);
    }

    #[test]
    fn git_status_cheap_reports_upstream_sha() {
        // The crux: the field has to ride the *cheap* path, since that is the one
        // the streamed snapshot is built from. Costing a config lookup and a refs
        // read — no revwalk — it clears the bar #1306 set, unlike the divergence
        // walk still absent here.
        let dir = tempfile::tempdir().unwrap();
        let repo = diverging_repo(dir.path());
        let status = git_status_cheap(dir.path());
        let upstream = repo
            .find_branch("origin/main", git2::BranchType::Remote)
            .unwrap()
            .get()
            .target()
            .unwrap();
        assert_eq!(
            status.upstream_sha.as_deref(),
            Some(upstream.to_string().as_str())
        );
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
    }

    #[test]
    fn folder_ahead_behind_computes_divergence_and_degrades() {
        // A diverging tracking branch → the on-demand walk reports (ahead, behind).
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());
        assert_eq!(folder_ahead_behind(dir.path()), Some((1, 1)));

        // A branch with no upstream → None (the tree renders no sync indicator).
        let no_up = tempfile::tempdir().unwrap();
        let repo = init_repo(no_up.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        assert_eq!(folder_ahead_behind(no_up.path()), None);

        // A detached HEAD and a plain (non-repo) directory → None.
        let detached = tempfile::tempdir().unwrap();
        let drepo = init_repo(detached.path());
        let a = empty_commit(&drepo, Some("refs/heads/main"), &[], "A");
        drepo.set_head_detached(a).unwrap();
        assert_eq!(folder_ahead_behind(detached.path()), None);
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(folder_ahead_behind(plain.path()), None);
    }

    #[tokio::test]
    async fn ahead_behind_op_returns_divergence_keyed_by_path_and_omits_no_upstream() {
        let diverging = tempfile::tempdir().unwrap();
        let _d = diverging_repo(diverging.path());
        let no_up = tempfile::tempdir().unwrap();
        let repo = init_repo(no_up.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();

        let svc = WorktreesService::new();
        let diverging_path = diverging.path().display().to_string();
        let no_up_path = no_up.path().display().to_string();
        let reply = svc
            .handle(
                "ahead-behind",
                json!({ "paths": [&diverging_path, &no_up_path] }),
            )
            .await
            .unwrap();
        let results = reply.get("results").unwrap();
        // The diverging worktree carries its counts, keyed by the requested path.
        let d = results.get(diverging_path.as_str()).unwrap();
        assert_eq!(d.get("ahead").and_then(Value::as_u64), Some(1));
        assert_eq!(d.get("behind").and_then(Value::as_u64), Some(1));
        // The no-upstream worktree is omitted entirely, not reported as zero.
        assert!(results.get(no_up_path.as_str()).is_none(), "{results:?}");

        // A missing/empty `paths` list yields an empty results object, not an error.
        let empty = svc.handle("ahead-behind", json!({})).await.unwrap();
        assert_eq!(empty.get("results"), Some(&json!({})));
    }

    #[tokio::test]
    async fn tree_snapshot_omits_ahead_behind_for_a_diverging_worktree() {
        // A window on a repo whose branch is 1 ahead of / 1 behind its upstream.
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "x" }),
        )
        .await
        .unwrap();

        let repos = repos_of(&svc.handle("tree", Value::Null).await.unwrap());
        let worktrees = repos[0].get("worktrees").and_then(Value::as_array).unwrap();
        let main_wt = &worktrees[0];
        // The cheap parts are present, but divergence is not — it is fetched
        // lazily via the `ahead-behind` op (#1306).
        assert_eq!(main_wt.get("branch").and_then(Value::as_str), Some("main"));
        assert!(main_wt.get("ahead").is_none(), "{main_wt:?}");
        assert!(main_wt.get("behind").is_none(), "{main_wt:?}");
    }

    #[tokio::test]
    async fn tree_snapshot_carries_head_sha_so_a_commit_is_a_real_delta() {
        // The end-to-end shape of the #1337 freshness fix. The server pushes a
        // snapshot only when it differs from the last one
        // (`server.rs`: `if snap != last`), so anything invisible on the wire
        // cannot trigger a re-render. Committing must therefore move the payload.
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "x" }),
        )
        .await
        .unwrap();

        let before = svc.handle("tree", Value::Null).await.unwrap();
        let wt = &repos_of(&before)[0]["worktrees"][0];
        assert_eq!(
            wt.get("head_sha").and_then(Value::as_str),
            Some(a.to_string().as_str())
        );

        // Commit again: same branch, same paths, same open windows — pre-#1337 the
        // snapshot was byte-identical here and the push was dropped.
        let head = repo.find_commit(a).unwrap();
        let b = empty_commit(&repo, Some("refs/heads/main"), &[&head], "B");
        let after = svc.handle("tree", Value::Null).await.unwrap();
        assert_eq!(
            repos_of(&after)[0]["worktrees"][0]
                .get("head_sha")
                .and_then(Value::as_str),
            Some(b.to_string().as_str())
        );
        assert_ne!(before, after, "a commit must be a visible snapshot delta");
    }

    #[tokio::test]
    async fn tree_snapshot_omits_head_sha_for_an_unborn_repo() {
        // Wire-compat: an absent SHA is dropped entirely rather than sent as null,
        // matching the payload's `skip_serializing_if` convention.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "x" }),
        )
        .await
        .unwrap();
        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert!(wt.get("head_sha").is_none(), "{wt:?}");
    }

    // --- Upstream SHA on the snapshot (#1344) ------------------------------

    #[tokio::test]
    async fn tree_snapshot_carries_upstream_sha_so_a_push_is_a_real_delta() {
        // The end-to-end shape of the #1344 fix, one ref over from #1337. A push
        // moves neither the branch nor the local head, so `upstream_sha` is the
        // only field that can carry the news. Without it the snapshot serialised
        // byte-identically, `server.rs`'s `if snap != last` dropped the frame, no
        // window re-rendered, and the lazy ahead/behind was never re-fetched.
        let dir = tempfile::tempdir().unwrap();
        let repo = diverging_repo(dir.path());
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "x" }),
        )
        .await
        .unwrap();

        let before = svc.handle("tree", Value::Null).await.unwrap();
        let head = repo.head().unwrap().target().unwrap();
        assert_ne!(
            repos_of(&before)[0]["worktrees"][0]
                .get("upstream_sha")
                .and_then(Value::as_str),
            Some(head.to_string().as_str()),
            "the fixture must start un-pushed for this to prove anything"
        );

        // Push: only `refs/remotes/origin/main` moves.
        simulate_push(&repo, head);
        let after = svc.handle("tree", Value::Null).await.unwrap();
        let wt = &repos_of(&after)[0]["worktrees"][0];
        assert_eq!(
            wt.get("upstream_sha").and_then(Value::as_str),
            Some(head.to_string().as_str())
        );
        // The head and branch are untouched across the push — so this delta rests
        // entirely on `upstream_sha`.
        assert_eq!(
            wt.get("head_sha").and_then(Value::as_str),
            repos_of(&before)[0]["worktrees"][0]
                .get("head_sha")
                .and_then(Value::as_str)
        );
        assert_ne!(before, after, "a push must be a visible snapshot delta");
    }

    #[tokio::test]
    async fn tree_snapshot_omits_upstream_sha_without_an_upstream() {
        // Wire-compat, and the no-regression case: a branch tracking nothing sends
        // no key at all rather than a null, so an older client sees exactly the
        // payload it saw before and still renders no sync indicator.
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "x" }),
        )
        .await
        .unwrap();
        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert!(wt.get("upstream_sha").is_none(), "{wt:?}");
        // The head still rides, so this is specifically the upstream degrading.
        assert!(wt.get("head_sha").is_some(), "{wt:?}");
    }

    // --- PR badge poller (#1337) -------------------------------------------

    /// Writes an executable stub that ignores its arguments and prints `stdout`,
    /// standing in for `gh api graphql` so the poll loop is exercised offline.
    /// Returns the shim lock alongside the path: the caller **must** hold the
    /// guard until the poller has finished exec'ing the stub. Writing an
    /// executable and then `execve`ing it races every other thread that forks —
    /// the child inherits the still-open writable FD and the exec fails
    /// `ETXTBSY`. See [`crate::pr_status`]'s twin helper (#642, #1344).
    fn fake_gh(dir: &Path, stdout: &str) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = shim_lock();
        let path = dir.join("fake-gh");
        write_exec_script(&path, &format!("#!/bin/sh\ncat <<'JSON'\n{stdout}\nJSON\n"));
        (path, guard)
    }

    /// A repo with a GitHub origin and one commit on `main`.
    fn github_repo(dir: &Path) -> Repository {
        let repo = init_repo(dir);
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        repo.remote("origin", "git@github.com:rust-works/omni-dev.git")
            .unwrap();
        repo
    }

    /// A pending badge whose verdict is about `head_oid`. The commit is explicit
    /// because the fold downgrades a badge naming a different commit than the
    /// worktree's HEAD (#1337) — a fixture that got it wrong would pass for the
    /// wrong reason.
    fn pending_badge(number: u64, head_oid: &str) -> PrBadge {
        PrBadge {
            number,
            is_draft: false,
            checks: PrCheckState::Pending,
            url: "u".into(),
            head_oid: head_oid.to_string(),
        }
    }

    #[test]
    fn pr_targets_from_snapshot_reads_github_branches_and_dedupes() {
        let snapshot = json!({"repos":[
            {
                "main_repo":"omni-dev",
                "github":{"owner":"rust-works","name":"omni-dev"},
                "root":"/r",
                // Two worktrees on the same branch must ask once, not twice.
                "worktrees":[
                    {"path":"/r","branch":"main","is_main":true,"open":true},
                    {"path":"/w1","branch":"main","is_main":false,"open":true},
                    {"path":"/w2","branch":"feature","is_main":false,"open":true},
                    // Detached: no branch, so nothing to resolve.
                    {"path":"/w3","is_main":false,"open":true}
                ]
            },
            {
                // Not on GitHub: contributes no targets at all.
                "main_repo":"local","root":"/l",
                "worktrees":[{"path":"/l","branch":"main","is_main":true,"open":true}]
            }
        ]});
        let targets = pr_targets_from_snapshot(&snapshot);
        assert_eq!(
            targets,
            vec![
                PrTarget {
                    owner: "rust-works".into(),
                    name: "omni-dev".into(),
                    branch: "feature".into()
                },
                PrTarget {
                    owner: "rust-works".into(),
                    name: "omni-dev".into(),
                    branch: "main".into()
                },
            ]
        );
    }

    #[test]
    fn pr_targets_from_snapshot_is_empty_without_repos() {
        assert!(pr_targets_from_snapshot(&json!({"repos":[]})).is_empty());
        assert!(pr_targets_from_snapshot(&json!({})).is_empty());
    }

    #[test]
    fn pr_targets_from_snapshot_skips_a_malformed_github_identity() {
        // Defensive: a `github` object without usable owner/name strings yields no
        // target rather than a half-built query.
        for github in [
            json!({}),
            json!({"owner": "o"}),
            json!({"owner": 1, "name": 2}),
        ] {
            let snapshot = json!({"repos":[{
                "main_repo":"r","github":github,"root":"/r",
                "worktrees":[{"path":"/r","branch":"main","is_main":true,"open":true}]
            }]});
            assert!(
                pr_targets_from_snapshot(&snapshot).is_empty(),
                "{snapshot:?}"
            );
        }
    }

    #[test]
    fn pr_should_fetch_on_a_moved_tree_or_an_elapsed_backoff() {
        let backoff = Duration::from_secs(600);
        // Never fetched: go.
        assert!(pr_should_fetch(false, None, backoff));
        // Quiet tree, backoff not elapsed: this is the common tick — wake, look,
        // spend nothing.
        assert!(!pr_should_fetch(
            false,
            Some(Duration::from_secs(1)),
            backoff
        ));
        // Quiet tree, backoff elapsed: time to look again.
        assert!(pr_should_fetch(false, Some(backoff), backoff));
        assert!(pr_should_fetch(false, Some(backoff * 2), backoff));
        // The load-bearing case: the tree moved (a commit landed), so fetch **now**
        // regardless of how deep the backoff had grown. Without this a push waits out
        // the full ceiling on a stale badge.
        assert!(pr_should_fetch(true, Some(Duration::ZERO), backoff));
        assert!(pr_should_fetch(
            true,
            Some(Duration::from_millis(1)),
            backoff
        ));
    }

    #[test]
    fn next_pr_poll_delay_holds_fast_while_pending_and_backs_off_to_the_ceiling() {
        let base = Duration::from_secs(10);
        // Something is still running: keep the watch cadence, however long we had
        // backed off for.
        assert_eq!(next_pr_poll_delay(base, base, true), base);
        assert_eq!(next_pr_poll_delay(MAX_PR_POLL_INTERVAL, base, true), base);
        // Everything terminal: double…
        assert_eq!(next_pr_poll_delay(base, base, false), base * 2);
        assert_eq!(next_pr_poll_delay(base * 2, base, false), base * 4);
        // …but never past the ceiling, and never overflow off it.
        assert_eq!(
            next_pr_poll_delay(MAX_PR_POLL_INTERVAL, base, false),
            MAX_PR_POLL_INTERVAL
        );
        assert_eq!(
            next_pr_poll_delay(Duration::MAX, base, false),
            MAX_PR_POLL_INTERVAL
        );
    }

    #[tokio::test]
    async fn tree_snapshot_folds_cached_pr_badges_onto_matching_branches() {
        let dir = tempfile::tempdir().unwrap();
        let repo = github_repo(dir.path());
        let head = repo.head().unwrap().target().unwrap().to_string();
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();

        // No poll has landed: the badge is absent, exactly as a pre-#1337 daemon.
        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert!(wt.get("pr").is_none(), "{wt:?}");

        // Seed the cache the poller writes, then re-read the tree.
        let mut badges = HashMap::new();
        badges.insert(
            PrTarget {
                owner: "rust-works".into(),
                name: "omni-dev".into(),
                branch: "main".into(),
            },
            pending_badge(1337, &head),
        );
        assert!(svc.pr_cache.replace(badges));

        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert_eq!(wt["pr"]["number"], json!(1337));
        assert_eq!(wt["pr"]["checks"], json!("pending"));
        // camelCase on the wire, or the extension silently loses the draft marker.
        assert_eq!(wt["pr"]["isDraft"], json!(false));
    }

    #[tokio::test]
    async fn tree_snapshot_omits_a_badge_for_a_detached_worktree() {
        // A detached HEAD has a commit but no branch, and a badge is keyed by
        // branch — so there is nothing to match. It must fall through silently
        // rather than borrow a badge from whatever branch happens to be cached, and
        // rather than sink the tree.
        let dir = tempfile::tempdir().unwrap();
        let repo = github_repo(dir.path());
        let head = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(head).unwrap();

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();
        // A badge *is* cached for `main` — the branch this worktree was on before
        // detaching. It must not leak onto the now-branchless row.
        let mut badges = HashMap::new();
        badges.insert(
            PrTarget {
                owner: "rust-works".into(),
                name: "omni-dev".into(),
                branch: "main".into(),
            },
            pending_badge(1, &head.to_string()),
        );
        svc.pr_cache.replace(badges);

        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert!(wt.get("branch").is_none(), "{wt:?}");
        // The SHA still shows — detached means no branch, not no commit.
        assert_eq!(
            wt.get("head_sha").and_then(Value::as_str),
            Some(head.to_string().as_str())
        );
        assert!(wt.get("pr").is_none(), "{wt:?}");
    }

    #[tokio::test]
    async fn tree_snapshot_omits_a_badge_for_an_unmatched_branch() {
        let dir = tempfile::tempdir().unwrap();
        github_repo(dir.path());
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();
        // A badge for a different branch must not leak onto `main`.
        let mut badges = HashMap::new();
        badges.insert(
            PrTarget {
                owner: "rust-works".into(),
                name: "omni-dev".into(),
                branch: "other".into(),
            },
            pending_badge(1, "irrelevant"),
        );
        svc.pr_cache.replace(badges);
        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert!(wt.get("pr").is_none(), "{wt:?}");
    }

    #[tokio::test]
    async fn pr_poller_asks_nothing_while_no_window_is_registered() {
        // The idle case — the daemon runs all day with no editor open. Point it at a
        // stub that fails loudly if ever spawned: a poll here would both waste
        // GitHub budget and, on a real `gh`, wake the radio for nothing.
        let bin_dir = tempfile::tempdir().unwrap();
        let marker = bin_dir.path().join("spawned");
        let fake = bin_dir.path().join("fake-gh");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\ntouch '{}'\necho '{{}}'\n", marker.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&fake, perms).unwrap();

        let svc = WorktreesService::new();
        svc.start_pr_poller_with(Duration::from_millis(20), fake);
        tokio::time::sleep(Duration::from_millis(200)).await;
        svc.shutdown().await;
        assert!(
            !marker.exists(),
            "the poller must not spawn gh with no windows registered"
        );
    }

    #[tokio::test]
    async fn pr_poller_survives_a_failing_gh_and_keeps_the_last_good_badges() {
        // Badges are decoration: an unauthenticated or broken `gh` must never sink
        // the tree, and one bad poll must not blank rows that were fine a second ago.
        let dir = tempfile::tempdir().unwrap();
        let repo = github_repo(dir.path());
        let head = repo.head().unwrap().target().unwrap().to_string();
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-gh");
        // Exits non-zero, exactly as `gh` does without `gh auth login`.
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'gh: not authenticated' >&2\nexit 1\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&fake, perms).unwrap();

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();
        // Seed a badge as though an earlier poll had succeeded.
        let mut seeded = HashMap::new();
        seeded.insert(
            PrTarget {
                owner: "rust-works".into(),
                name: "omni-dev".into(),
                branch: "main".into(),
            },
            pending_badge(7, &head),
        );
        svc.pr_cache.replace(seeded);

        svc.start_pr_poller_with(Duration::from_millis(20), fake);
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The tree still serves, and the seeded badge survived the failing polls.
        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert_eq!(wt["pr"]["number"], json!(7));
        svc.shutdown().await;
    }

    #[tokio::test]
    // The shim guard is deliberately held across the awaits below: it must span
    // both the stub's write *and* the poller's exec of it, since the ETXTBSY race
    // is against another test writing while this one forks. Safe here — only test
    // threads take it, never a task inside the runtime, so it cannot deadlock.
    // Scoped per-test rather than on the module, which would also silence the
    // registry lock's "never held across .await" invariant.
    #[allow(clippy::await_holding_lock)]
    async fn pr_poller_wakes_when_the_first_window_opens_after_an_idle_start() {
        // The normal startup order: the daemon starts at login, *before* any
        // editor. It therefore sees an empty tree and backs off to the 30-minute
        // ceiling — so unless a register wakes it, the first badge of the session
        // arrives up to half an hour after the window does, which reads as the
        // feature being broken rather than slow.
        let dir = tempfile::tempdir().unwrap();
        github_repo(dir.path());
        let bin_dir = tempfile::tempdir().unwrap();
        let (fake, _shim) = fake_gh(
            bin_dir.path(),
            r#"{"data":{"r0":{"b0":{
                "target":{"oid":"a","statusCheckRollup":{"contexts":{"nodes":[
                  {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":null}
                ]}}},
                "associatedPullRequests":{"nodes":[{"number":99,"isDraft":false,"url":"u"}]}
            }}}}"#,
        );

        let svc = WorktreesService::new();
        // Poller first, with nothing registered — it backs off on the empty tree.
        svc.start_pr_poller_with(Duration::from_millis(50), fake);
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Now an editor opens.
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();

        // The badge must follow promptly — the register wakes the loop out of its
        // backoff. The deadline is orders of magnitude below the ceiling, so this
        // fails on the bug rather than merely being slow.
        let badge = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(badge) = svc.pr_cache.get("rust-works", "omni-dev", "main") {
                    return badge;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("a window opening must wake the poller out of its idle backoff");
        assert_eq!(badge.number, 99);
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn a_commit_invalidates_the_previous_verdict_without_a_poll() {
        // The acceptance criterion: "pushing a new commit invalidates the badge
        // rather than leaving the previous head's verdict standing."
        //
        // The cache still holds the verdict for the *old* commit, and the poller may
        // have backed off for up to half an hour. So the fold — which runs on every
        // snapshot — has to notice on its own, with no network call.
        let dir = tempfile::tempdir().unwrap();
        let repo = github_repo(dir.path());
        let first = repo.head().unwrap().target().unwrap();

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();

        // A green verdict, correctly describing the commit currently checked out.
        let mut badges = HashMap::new();
        badges.insert(
            PrTarget {
                owner: "rust-works".into(),
                name: "omni-dev".into(),
                branch: "main".into(),
            },
            PrBadge {
                number: 1337,
                is_draft: false,
                checks: PrCheckState::Success,
                url: "u".into(),
                head_oid: first.to_string(),
            },
        );
        svc.pr_cache.replace(badges);

        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert_eq!(
            wt["pr"]["checks"],
            json!("success"),
            "green for its own commit"
        );

        // Now commit — as a push would leave things, with the cache untouched.
        let head = repo.find_commit(first).unwrap();
        empty_commit(&repo, Some("refs/heads/main"), &[&head], "B");

        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert_eq!(
            wt["pr"]["checks"],
            json!("pending"),
            "the previous commit's ✓ must not stand after a new commit"
        );
        // The PR itself is still shown — it is the *verdict* that is unknown, not
        // the PR.
        assert_eq!(wt["pr"]["number"], json!(1337));
    }

    #[test]
    fn is_stale_for_compares_the_commit_the_verdict_describes() {
        let badge = pending_badge(1, "aaa");
        assert!(!badge.is_stale_for(Some("aaa")));
        assert!(badge.is_stale_for(Some("bbb")));
        // No local HEAD (unborn): nothing to compare against, so not stale.
        assert!(!badge.is_stale_for(None));
    }

    #[test]
    fn pr_watch_tracks_the_head_so_a_commit_is_visible_to_the_poller() {
        let snap = |sha: &str| {
            json!({"repos":[{
                "main_repo":"omni-dev",
                "github":{"owner":"rust-works","name":"omni-dev"},
                "root":"/r",
                "worktrees":[{"path":"/r","branch":"main","head_sha":sha,"is_main":true,"open":true}]
            }]})
        };
        let before = pr_watch_from_snapshot(&snap("aaa"));
        let after = pr_watch_from_snapshot(&snap("bbb"));
        // Same target, different head — the poller's "something moved" signal.
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].target, after[0].target);
        assert_ne!(before, after);
        // Identical trees compare equal, so a quiet tick asks nothing.
        assert_eq!(before, pr_watch_from_snapshot(&snap("aaa")));
    }

    #[test]
    fn pr_watch_tracks_the_upstream_so_a_push_is_visible_to_the_poller() {
        // #1344's bonus. A push is what *starts* the CI run a badge reports, yet
        // it moves no local head — so watching only `head_sha` left the poller
        // blind to it and the badge sat at `●` until the backoff elapsed, up to
        // its 30-minute ceiling.
        let snap = |upstream: &str| {
            json!({"repos":[{
                "main_repo":"omni-dev",
                "github":{"owner":"rust-works","name":"omni-dev"},
                "root":"/r",
                "worktrees":[{"path":"/r","branch":"main","head_sha":"aaa",
                              "upstream_sha":upstream,"is_main":true,"open":true}]
            }]})
        };
        let before = pr_watch_from_snapshot(&snap("aaa"));
        let after = pr_watch_from_snapshot(&snap("bbb"));
        // Same target, same head — only the upstream moved, and that alone must
        // register as "go and ask now".
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].target, after[0].target);
        assert_eq!(before[0].head_sha, after[0].head_sha);
        assert_ne!(before, after);
        // A quiet tick still asks nothing.
        assert_eq!(before, pr_watch_from_snapshot(&snap("aaa")));
    }

    #[test]
    fn pr_watch_omits_an_absent_upstream_rather_than_erroring() {
        // An older daemon — or any branch tracking nothing — simply sends no
        // `upstream_sha`, which reads as `None` rather than failing the poll.
        let snap = json!({"repos":[{
            "main_repo":"omni-dev",
            "github":{"owner":"rust-works","name":"omni-dev"},
            "root":"/r",
            "worktrees":[{"path":"/r","branch":"main","head_sha":"aaa","is_main":true,"open":true}]
        }]});
        let watch = pr_watch_from_snapshot(&snap);
        assert_eq!(watch.len(), 1);
        assert_eq!(watch[0].upstream_sha, None);
        assert_eq!(watch[0].head_sha.as_deref(), Some("aaa"));
    }

    #[test]
    fn start_pr_poller_is_a_noop_outside_a_runtime() {
        let svc = WorktreesService::new();
        svc.start_pr_poller();
        assert!(svc
            .poller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none());
    }

    #[tokio::test]
    async fn start_pr_poller_is_idempotent_and_shutdown_stops_it() {
        let svc = WorktreesService::new();
        svc.start_pr_poller_with(Duration::from_millis(50), PathBuf::from("/bin/true"));
        let token = svc
            .poller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|t| t.token.clone())
            .expect("poller started");

        // Cancel the live task, then start again: if `start` spawned a replacement
        // it would orphan this one, so the token staying cancelled proves it did not.
        token.cancel();
        svc.start_pr_poller_with(Duration::from_millis(50), PathBuf::from("/bin/true"));
        assert!(svc
            .poller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|t| t.token.is_cancelled()));

        svc.shutdown().await;
        assert!(svc
            .poller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none());
    }

    #[tokio::test]
    // Holds the shim guard across awaits; see the note above.
    #[allow(clippy::await_holding_lock)]
    async fn pr_poller_resolves_via_gh_populates_the_cache_and_stops_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        github_repo(dir.path());
        let bin_dir = tempfile::tempdir().unwrap();
        // One repo, one branch → aliases r0/b0. A still-running check so the badge
        // stays pending and the loop keeps its fast cadence.
        let (fake, _shim) = fake_gh(
            bin_dir.path(),
            r#"{"data":{"r0":{"b0":{
                "target":{"oid":"abc","statusCheckRollup":{"contexts":{"nodes":[
                  {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":null}
                ]}}},
                "associatedPullRequests":{"nodes":[{"number":1337,"isDraft":false,"url":"http://x/1337"}]}
            }}}}"#,
        );
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();
        svc.start_pr_poller_with(Duration::from_millis(50), fake.clone());

        // Wait on a generous wall-clock deadline: each poll spawns a real
        // subprocess, and under a loaded machine (a full `build.sh` runs a build
        // and clippy alongside) a tight budget flakes rather than fails honestly.
        let badge = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(badge) = svc.pr_cache.get("rust-works", "omni-dev", "main") {
                    return badge;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("poller should resolve a badge through the fake gh");
        assert_eq!(badge.number, 1337);
        assert_eq!(badge.checks, crate::pr_status::PrCheckState::Pending);

        // The badge reaches the wire the windows actually read.
        let wt = &repos_of(&svc.handle("tree", Value::Null).await.unwrap())[0]["worktrees"][0];
        assert_eq!(wt["pr"]["number"], json!(1337));

        // And the loop is quiescent after shutdown: the generation must stop moving.
        svc.shutdown().await;
        let generation = svc.registry.change_generation();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            svc.registry.change_generation(),
            generation,
            "no bumps after shutdown"
        );
    }

    #[tokio::test]
    // Holds the shim guard across awaits; see the note above.
    #[allow(clippy::await_holding_lock)]
    async fn pr_poller_bumps_only_when_a_verdict_actually_moves() {
        // The diff-and-drop contract: an unchanged poll must not bump, or every
        // window re-renders on every tick — the cost this design exists to avoid.
        let dir = tempfile::tempdir().unwrap();
        github_repo(dir.path());
        let bin_dir = tempfile::tempdir().unwrap();
        let (fake, _shim) = fake_gh(
            bin_dir.path(),
            r#"{"data":{"r0":{"b0":{
                "target":{"oid":"abc","statusCheckRollup":{"contexts":{"nodes":[
                  {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":null}
                ]}}},
                "associatedPullRequests":{"nodes":[{"number":1,"isDraft":false,"url":"u"}]}
            }}}}"#,
        );
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w", "folders": [dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();
        svc.start_pr_poller_with(Duration::from_millis(50), fake.clone());

        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if svc.pr_cache.get("rust-works", "omni-dev", "main").is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("poller should resolve a badge through the fake gh");
        // The fake always answers identically, so after the first resolve every
        // subsequent poll is a no-change and must leave the generation alone.
        let settled = svc.registry.change_generation();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            svc.registry.change_generation(),
            settled,
            "an unchanged poll must not bump the change-notify"
        );
        svc.shutdown().await;
    }

    #[test]
    fn sync_indicator_formats_only_with_upstream() {
        assert_eq!(sync_indicator(Some(2), Some(1)).as_deref(), Some("(+2 -1)"));
        assert_eq!(sync_indicator(Some(0), Some(0)).as_deref(), Some("(+0 -0)"));
        assert_eq!(sync_indicator(None, None), None);
        // A partial pair (no real upstream) yields nothing.
        assert_eq!(sync_indicator(Some(1), None), None);
    }

    #[tokio::test]
    async fn list_enriches_entries_with_git_status() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [dir.path()], "repo": "r" }),
        )
        .await
        .unwrap();
        let payload = svc.handle("list", Value::Null).await.unwrap();
        let windows = windows_of(&payload);
        assert_eq!(windows.len(), 1);
        assert_eq!(
            windows[0].get("branch").and_then(Value::as_str),
            Some("main")
        );
        // No upstream configured → the ahead/behind keys are absent, not zero.
        assert!(windows[0].get("ahead").is_none());
        assert!(windows[0].get("behind").is_none());
        // The main repo name is enriched onto the entry.
        assert_eq!(
            windows[0].get("main_repo").and_then(Value::as_str),
            dir.path().file_name().and_then(|n| n.to_str())
        );

        // A non-repo folder is still listed, just without a branch or main repo.
        let plain = tempfile::tempdir().unwrap();
        svc.handle(
            "register",
            json!({ "key": "w2", "folders": [plain.path()], "repo": "plain" }),
        )
        .await
        .unwrap();
        let windows = windows_of(&svc.handle("list", Value::Null).await.unwrap()).clone();
        let w2 = windows
            .iter()
            .find(|w| w.get("key").and_then(Value::as_str) == Some("w2"))
            .unwrap();
        assert!(w2.get("branch").is_none());
        assert!(w2.get("main_repo").is_none());
    }

    #[test]
    fn window_label_prefers_git_branch_over_title() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let repo_name = dir.path().file_name().unwrap().to_str().unwrap();
        let entry = WindowEntry {
            key: "k".to_string(),
            folders: vec![dir.path().to_path_buf()],
            // Both the companion `repo` and `title` are overridden by the
            // git-derived main repo name and computed branch.
            repo: Some("companion-repo".to_string()),
            title: Some("ignored title".to_string()),
            pid: None,
            last_seen: Utc::now(),
        };
        // Main checkout: `repo · branch`, and with no upstream there is no sync.
        assert_eq!(window_label(&entry), format!("{repo_name} · main"));
    }

    #[tokio::test]
    async fn list_includes_ahead_behind_for_tracking_branch() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [dir.path()], "repo": "r" }),
        )
        .await
        .unwrap();
        let payload = svc.handle("list", Value::Null).await.unwrap();
        let windows = windows_of(&payload);
        // A tracking branch serializes branch plus both divergence counts.
        assert_eq!(
            windows[0].get("branch").and_then(Value::as_str),
            Some("main")
        );
        assert_eq!(windows[0].get("ahead").and_then(Value::as_u64), Some(1));
        assert_eq!(windows[0].get("behind").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn window_label_includes_sync_for_tracking_branch() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());
        let repo_name = dir.path().file_name().unwrap().to_str().unwrap();
        let entry = WindowEntry {
            key: "k".to_string(),
            folders: vec![dir.path().to_path_buf()],
            repo: Some("companion-repo".to_string()),
            title: None,
            pid: None,
            last_seen: Utc::now(),
        };
        // A tracking branch appends the `(+ahead -behind)` sync indicator.
        assert_eq!(window_label(&entry), format!("{repo_name} · main (+1 -1)"));
    }

    /// Adds a linked worktree of `repo` at `wt_path` checked out on a new
    /// `branch` pointed at `base`, mirroring `git worktree add -b <branch>
    /// <wt_path>`.
    fn add_worktree(repo: &Repository, base: git2::Oid, wt_path: &Path, branch: &str) {
        let commit = repo.find_commit(base).unwrap();
        repo.branch(branch, &commit, false).unwrap();
        let reference = repo
            .find_reference(&format!("refs/heads/{branch}"))
            .unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree(branch, wt_path, Some(&opts)).unwrap();
    }

    #[test]
    fn git_status_marks_linked_worktree_and_names_parent_repo() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();

        // A linked worktree checked out on a new `feature` branch, in a
        // directory whose basename is deliberately *not* the repo name.
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");

        let status = git_status(&wt_path);
        assert!(status.is_worktree);
        assert_eq!(status.branch.as_deref(), Some("feature"));
        // The worktree names its *parent* repo, not its worktree-folder basename.
        assert_eq!(
            status.main_repo.as_deref(),
            main_dir.path().file_name().and_then(|n| n.to_str())
        );

        // The main checkout resolves the same repo name and is not a worktree.
        let main_status = git_status(main_dir.path());
        assert!(!main_status.is_worktree);
        assert_eq!(main_status.main_repo, status.main_repo);
    }

    #[test]
    fn window_label_marks_worktree_with_fork_glyph() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");

        let repo_name = main_dir.path().file_name().unwrap().to_str().unwrap();
        let entry = WindowEntry {
            key: "k".to_string(),
            folders: vec![wt_path],
            repo: Some("feature-wt".to_string()),
            title: None,
            pid: None,
            last_seen: Utc::now(),
        };
        // A worktree line: parent repo, the fork glyph, then the branch (no
        // upstream here, so no sync suffix).
        assert_eq!(window_label(&entry), format!("{repo_name} ⑂ feature"));
    }

    #[test]
    fn main_repo_name_derives_from_common_dir() {
        // Normal layout: the repo is the directory that contains `.git`.
        assert_eq!(
            main_repo_name(Path::new("/home/me/omni-dev/.git")).as_deref(),
            Some("omni-dev")
        );
        // A trailing slash on the common dir does not change the answer.
        assert_eq!(
            main_repo_name(Path::new("/home/me/omni-dev/.git/")).as_deref(),
            Some("omni-dev")
        );
        // A bare repo: its own directory name, without the `.git` suffix.
        assert_eq!(
            main_repo_name(Path::new("/srv/git/omni-dev.git")).as_deref(),
            Some("omni-dev")
        );
        // A `.git` at the filesystem root has no parent name to use.
        assert_eq!(main_repo_name(Path::new("/.git")), None);
    }

    // --- Repo/worktree tree (#1265) ----------------------------------------

    /// Pulls the `repos` array out of a `tree` payload (owned, so it survives a
    /// temporary payload).
    fn repos_of(payload: &Value) -> Vec<Value> {
        payload
            .get("repos")
            .and_then(Value::as_array)
            .expect("repos array")
            .clone()
    }

    fn github(owner: &str, name: &str) -> Option<GithubIdentity> {
        Some(GithubIdentity {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    #[test]
    fn github_identity_parses_supported_forms() {
        // https / http, with and without the `.git` suffix.
        assert_eq!(
            github_identity("https://github.com/rust-works/omni-dev.git"),
            github("rust-works", "omni-dev")
        );
        assert_eq!(
            github_identity("https://github.com/rust-works/omni-dev"),
            github("rust-works", "omni-dev")
        );
        assert_eq!(github_identity("http://github.com/o/r"), github("o", "r"));
        // SCP-like and ssh:// / git:// forms.
        assert_eq!(
            github_identity("git@github.com:rust-works/omni-dev.git"),
            github("rust-works", "omni-dev")
        );
        assert_eq!(
            github_identity("ssh://git@github.com/o/r.git"),
            github("o", "r")
        );
        assert_eq!(github_identity("git://github.com/o/r"), github("o", "r"));
        // A trailing slash and surrounding whitespace are tolerated.
        assert_eq!(
            github_identity("  https://github.com/o/r/  "),
            github("o", "r")
        );
    }

    #[test]
    fn github_identity_rejects_non_github_and_malformed() {
        // Non-GitHub hosts.
        assert_eq!(github_identity("https://gitlab.com/o/r.git"), None);
        assert_eq!(github_identity("git@example.com:o/r.git"), None);
        // Missing or extra path segments.
        assert_eq!(github_identity("https://github.com/onlyowner"), None);
        assert_eq!(github_identity("https://github.com/o/r/extra"), None);
        assert_eq!(github_identity("https://github.com/"), None);
        // Not a URL at all.
        assert_eq!(github_identity("not a url"), None);
    }

    #[test]
    fn remote_github_identity_reads_origin_then_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        // No remotes → None.
        assert_eq!(remote_github_identity(&repo), None);
        // A non-GitHub origin is not a match.
        repo.remote("origin", "https://gitlab.com/o/r.git").unwrap();
        assert_eq!(remote_github_identity(&repo), None);
        // A GitHub origin resolves to its identity.
        repo.remote_set_url("origin", "git@github.com:rust-works/omni-dev.git")
            .unwrap();
        assert_eq!(
            remote_github_identity(&repo),
            github("rust-works", "omni-dev")
        );

        // Origin non-GitHub but another remote is GitHub: the fallback loop over
        // the remaining remotes finds it.
        repo.remote_set_url("origin", "https://gitlab.com/o/r.git")
            .unwrap();
        repo.remote("upstream", "https://github.com/other/proj.git")
            .unwrap();
        assert_eq!(remote_github_identity(&repo), github("other", "proj"));
    }

    #[tokio::test]
    async fn tree_is_empty_with_no_windows_and_skips_non_repos() {
        let svc = WorktreesService::new();
        // No windows → an empty repo set (not an error), toggle at its default.
        assert_eq!(
            svc.handle("tree", Value::Null).await.unwrap(),
            json!({ "repos": [], "show_closed": true })
        );
        // A plain non-repo folder is skipped rather than sinking the op.
        let plain = tempfile::tempdir().unwrap();
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [plain.path()], "repo": "plain" }),
        )
        .await
        .unwrap();
        assert!(repos_of(&svc.handle("tree", Value::Null).await.unwrap()).is_empty());
    }

    #[tokio::test]
    async fn tree_enumerates_main_and_linked_with_open_join_and_github() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        // A GitHub origin so the repo carries an identity in the payload.
        repo.remote("origin", "git@github.com:rust-works/omni-dev.git")
            .unwrap();

        // A linked worktree on a new `feature` branch, in a directory whose
        // basename is deliberately not the repo name.
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");

        let svc = WorktreesService::new();
        // A window open on the main checkout and one on the linked worktree —
        // two windows, but one repo (they must dedupe).
        svc.handle(
            "register",
            json!({ "key": "wm", "folders": [main_dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();
        svc.handle(
            "register",
            json!({ "key": "wf", "folders": [wt_path], "repo": "feature-wt" }),
        )
        .await
        .unwrap();

        let repos = repos_of(&svc.handle("tree", Value::Null).await.unwrap());
        assert_eq!(
            repos.len(),
            1,
            "two worktrees of one repo dedupe: {repos:?}"
        );
        let repo0 = &repos[0];
        // Repo identity is the parent-repo name (not a worktree-folder basename).
        assert_eq!(
            repo0.get("main_repo").and_then(Value::as_str),
            main_dir.path().file_name().and_then(|n| n.to_str())
        );
        assert_eq!(
            repo0.pointer("/github/owner").and_then(Value::as_str),
            Some("rust-works")
        );
        assert_eq!(
            repo0.pointer("/github/name").and_then(Value::as_str),
            Some("omni-dev")
        );
        assert!(repo0.get("root").and_then(Value::as_str).is_some());

        let worktrees = repo0.get("worktrees").and_then(Value::as_array).unwrap();
        assert_eq!(worktrees.len(), 2);
        // Main working tree first: is_main, open, with the main window's key.
        let main_wt = &worktrees[0];
        assert_eq!(main_wt.get("is_main").and_then(Value::as_bool), Some(true));
        assert_eq!(main_wt.get("open").and_then(Value::as_bool), Some(true));
        assert_eq!(
            main_wt.get("window_key").and_then(Value::as_str),
            Some("wm")
        );
        assert_eq!(main_wt.get("branch").and_then(Value::as_str), Some("main"));
        // Linked worktree: not main, open via the feature window.
        let linked = &worktrees[1];
        assert_eq!(linked.get("is_main").and_then(Value::as_bool), Some(false));
        assert_eq!(linked.get("open").and_then(Value::as_bool), Some(true));
        assert_eq!(linked.get("window_key").and_then(Value::as_str), Some("wf"));
        assert_eq!(
            linked.get("branch").and_then(Value::as_str),
            Some("feature")
        );
    }

    #[tokio::test]
    async fn tree_marks_unopened_linked_worktree_closed_and_omits_github() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        // No remote at all → the repo carries no `github` identity.
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");

        let svc = WorktreesService::new();
        // Only the main checkout has a window; the linked worktree has none.
        svc.handle(
            "register",
            json!({ "key": "wm", "folders": [main_dir.path()], "repo": "omni-dev" }),
        )
        .await
        .unwrap();

        let repos = repos_of(&svc.handle("tree", Value::Null).await.unwrap());
        assert_eq!(repos.len(), 1);
        assert!(repos[0].get("github").is_none(), "no remote → no github");
        let worktrees = repos[0].get("worktrees").and_then(Value::as_array).unwrap();
        let linked = worktrees
            .iter()
            .find(|w| w.get("is_main").and_then(Value::as_bool) == Some(false))
            .expect("the linked worktree");
        // Enumerated even though no window has it open, and marked closed.
        assert_eq!(linked.get("open").and_then(Value::as_bool), Some(false));
        assert!(linked.get("window_key").is_none());
    }

    // --- Close op (#1277) --------------------------------------------------

    /// Builds a repo whose main working tree is on `trunk` with one **clean**
    /// linked worktree on `feature`, returning the temp dirs (kept alive so the
    /// paths stay valid) and the linked worktree path.
    fn repo_with_linked_worktree() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/trunk"), &[], "A");
        repo.set_head("refs/heads/trunk").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");
        (main_dir, wt_parent, wt_path)
    }

    /// [`repo_with_linked_worktree`] with a **second** linked worktree of the same
    /// repo — the shape a multi-select delete fans out over, and the only one where
    /// two prunes share a `.git/worktrees` to race on (#1359).
    fn repo_with_two_linked_worktrees() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PathBuf)
    {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/trunk"), &[], "A");
        repo.set_head("refs/heads/trunk").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let first = wt_parent.path().join("first-wt");
        let second = wt_parent.path().join("second-wt");
        add_worktree(&repo, a, &first, "first");
        add_worktree(&repo, a, &second, "second");
        (main_dir, wt_parent, first, second)
    }

    #[tokio::test]
    async fn close_removes_two_linked_worktrees_of_one_repo_concurrently() {
        let (main_dir, _wtp, first, second) = repo_with_two_linked_worktrees();
        let svc = Arc::new(WorktreesService::new());

        // The multi-select fan-out: one `close` op per target, both in flight at
        // once against the one repo's shared admin state. Genuinely concurrent
        // even on this single-threaded runtime — each op's prune is a
        // `spawn_blocking`, so awaiting its join yields to the other op.
        //
        // This guards the fan-out end-to-end (both ops complete, neither is
        // starved or deadlocked by `prune_lock`); it is deliberately *not* sold as
        // a race detector for the lock, because it is not one — it passes with the
        // guard removed, the two prunes being far too quick to collide reliably.
        let close = |path: PathBuf| {
            let svc = svc.clone();
            async move {
                svc.handle(
                    "close",
                    json!({ "path": path, "remove": true, "confirmed": true }),
                )
                .await
            }
        };
        let (a, b) = tokio::join!(close(first.clone()), close(second.clone()));

        assert_eq!(a.unwrap(), json!({ "removed": true }));
        assert_eq!(b.unwrap(), json!({ "removed": true }));
        assert!(!first.exists());
        assert!(!second.exists());
        // Both *admin* entries pruned too, not merely the directories — the half
        // the two ops contend on.
        let repo = Repository::open(main_dir.path()).unwrap();
        assert!(repo.worktrees().unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_closes_overlap_their_heartbeat_waits() {
        let (_main, _wtp, first, second) = repo_with_two_linked_worktrees();
        let svc = Arc::new(WorktreesService::new());
        // Two *different* windows own the two targets — the multi-select shape.
        for (key, path) in [("w2", &first), ("w3", &second)] {
            svc.handle("register", json!({ "key": key, "folders": [path] }))
                .await
                .unwrap();
        }

        let spawn_close = |path: PathBuf| {
            let svc = svc.clone();
            tokio::spawn(async move {
                svc.handle(
                    "close",
                    json!({
                        "path": path,
                        "remove": true,
                        "confirmed": true,
                        "requester_key": "w1",
                    }),
                )
                .await
            })
        };
        let a = spawn_close(first.clone());
        let b = spawn_close(second.clone());

        // The crux of #1359, and the one thing pinning `prune_lock`'s placement:
        // *both* windows are told to close while *neither* op has finished, so the
        // two multi-second heartbeat waits are in flight at once. Take the guard
        // before `await_windows_closed` instead of after and this fails — op B
        // would sit on the lock without ever marking w3, restoring exactly the
        // N-stacked-waits latency the fan-out exists to remove.
        for key in ["w2", "w3"] {
            let mut saw_close = false;
            for _ in 0..400 {
                let hb = svc
                    .handle("heartbeat", json!({ "key": key }))
                    .await
                    .unwrap();
                if hb.get("close").and_then(Value::as_bool) == Some(true) {
                    saw_close = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(saw_close, "{key} should have been told to close while the other target's close was still waiting");
        }
        assert!(
            !a.is_finished() && !b.is_finished(),
            "neither close can have finished: both windows are still registered"
        );

        // Both windows close; both ops then remove.
        for key in ["w2", "w3"] {
            svc.handle("unregister", json!({ "key": key }))
                .await
                .unwrap();
        }
        assert_eq!(a.await.unwrap().unwrap(), json!({ "removed": true }));
        assert_eq!(b.await.unwrap().unwrap(), json!({ "removed": true }));
        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[tokio::test]
    async fn close_safety_check_reports_clean_linked_as_removable_with_no_risks() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        // Phase 1 (confirmed absent) on a clean linked worktree: removable, not
        // main, no risks → the extension proceeds with no dialog.
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        assert_eq!(report.get("removable").and_then(Value::as_bool), Some(true));
        assert_eq!(report.get("is_main").and_then(Value::as_bool), Some(false));
        assert_eq!(report.get("open").and_then(Value::as_bool), Some(false));
        assert!(report
            .get("risks")
            .and_then(Value::as_array)
            .unwrap()
            .is_empty());
        // No side effects: the worktree still exists.
        assert!(wt_path.exists());
    }

    #[tokio::test]
    async fn close_removes_a_clean_linked_worktree() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        let reply = svc
            .handle(
                "close",
                json!({ "path": wt_path, "remove": true, "confirmed": true }),
            )
            .await
            .unwrap();
        assert_eq!(reply, json!({ "removed": true }));
        assert!(
            !wt_path.exists(),
            "the worktree directory should be deleted"
        );
    }

    #[test]
    fn remove_worktree_deletes_the_directory_and_prunes_the_admin_metadata() {
        // The reorder (#1315) must still fully remove a worktree: both the
        // checked-out directory *and* the admin metadata git tracks it by, so it
        // no longer appears in `Repository::worktrees()`.
        let (main, _wtp, wt_path) = repo_with_linked_worktree();
        let admin = main.path().join(".git").join("worktrees").join("feature");
        assert!(admin.exists(), "admin metadata should exist before removal");

        remove_worktree(&wt_path).unwrap();

        assert!(!wt_path.exists(), "the working directory should be gone");
        assert!(!admin.exists(), "the admin metadata should be pruned");
        let main_repo = Repository::open(main.path()).unwrap();
        assert_eq!(
            main_repo.worktrees().unwrap().len(),
            0,
            "git should no longer track the worktree"
        );
    }

    #[test]
    fn remove_worktree_recovers_a_half_removed_orphan() {
        // The exact #1315 leftover: the old ordering deleted the admin metadata
        // first, then failed to rmdir the working tree, orphaning the directory
        // with a dangling `.git` gitlink. `remove_worktree` must clean it up
        // rather than error with "not a git worktree".
        let (main, _wtp, wt_path) = repo_with_linked_worktree();
        let admin = main.path().join(".git").join("worktrees").join("feature");
        // Simulate the half-removed state: admin gone, directory (+gitlink) left.
        std::fs::remove_dir_all(&admin).unwrap();
        assert!(wt_path.join(".git").is_file(), "dangling gitlink remains");
        assert!(
            Repository::open(&wt_path).is_err(),
            "the orphan should not open as a repo"
        );

        remove_worktree(&wt_path).unwrap();
        assert!(
            !wt_path.exists(),
            "the leftover directory should be removed"
        );
    }

    #[test]
    fn is_orphaned_worktree_only_matches_a_dangling_linked_gitlink() {
        let (main, _wtp, wt_path) = repo_with_linked_worktree();
        // A live worktree: gitlink resolves → not an orphan.
        assert!(!is_orphaned_worktree(&wt_path));
        // The main checkout has a `.git` directory → not an orphan.
        assert!(!is_orphaned_worktree(main.path()));
        // Drop the admin metadata → the gitlink now dangles → orphan.
        std::fs::remove_dir_all(main.path().join(".git").join("worktrees").join("feature"))
            .unwrap();
        assert!(is_orphaned_worktree(&wt_path));
    }

    #[test]
    fn remove_dir_all_retrying_is_idempotent_on_a_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("gone");
        assert!(remove_dir_all_retrying(&missing).is_ok());
    }

    #[test]
    fn is_transient_rmdir_error_matches_only_the_repopulated_directory_race() {
        use std::io::Error;
        for errno in [nix::libc::ENOTEMPTY, nix::libc::EEXIST, nix::libc::EBUSY] {
            assert!(
                is_transient_rmdir_error(&Error::from_raw_os_error(errno)),
                "errno {errno} is the concurrent-writer race and must be retried"
            );
        }
        // A hard failure must surface immediately rather than burn the backoff
        // waiting for a condition that will never clear.
        for errno in [
            nix::libc::EACCES,
            nix::libc::EPERM,
            nix::libc::EROFS,
            nix::libc::ENOTDIR,
        ] {
            assert!(
                !is_transient_rmdir_error(&Error::from_raw_os_error(errno)),
                "errno {errno} is permanent and must not be retried"
            );
        }
        // Not from the OS at all, so there is no errno to classify.
        assert!(!is_transient_rmdir_error(&Error::other("synthetic")));
    }

    #[test]
    fn remove_dir_all_retrying_surfaces_a_non_transient_error_without_retrying() {
        // Removing a *file* as if it were a directory fails with ENOTDIR: not the
        // race, so it must fail on the first attempt with the original cause
        // attached, leaving the path untouched.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();

        let mut attempts = 0;
        let err = remove_dir_all_retrying_with(&file, WORKTREE_RMDIR_BACKOFF, || {
            attempts += 1;
            std::fs::remove_dir_all(&file)
        })
        .unwrap_err();

        assert_eq!(attempts, 1, "a permanent error must not be retried");
        assert!(
            err.to_string()
                .contains("failed to remove worktree directory"),
            "unexpected error: {err:#}"
        );
        assert!(err.source().is_some(), "the io::Error cause is preserved");
        assert!(file.exists());
    }

    #[test]
    fn remove_dir_all_retrying_gives_up_after_the_backoff_is_exhausted() {
        // A writer that never quiesces: every sweep re-finds the directory
        // populated. Once the schedule runs out the ENOTEMPTY must surface rather
        // than the loop spinning forever.
        let tmp = tempfile::tempdir().unwrap();
        let mut attempts = 0;
        let backoff = [Duration::ZERO, Duration::ZERO];
        let err = remove_dir_all_retrying_with(tmp.path(), &backoff, || {
            attempts += 1;
            Err(std::io::Error::from_raw_os_error(nix::libc::ENOTEMPTY))
        })
        .unwrap_err();

        // One attempt per delay, plus the initial one.
        assert_eq!(attempts, backoff.len() + 1);
        assert!(
            err.to_string()
                .contains("failed to remove worktree directory"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn remove_dir_all_retrying_succeeds_once_the_writer_quiesces() {
        // The #1315 happy path, deterministically: the race clears partway through
        // the schedule and the removal then succeeds.
        let tmp = tempfile::tempdir().unwrap();
        let mut attempts = 0;
        let result = remove_dir_all_retrying_with(tmp.path(), WORKTREE_RMDIR_BACKOFF, || {
            attempts += 1;
            if attempts < 3 {
                Err(std::io::Error::from_raw_os_error(nix::libc::ENOTEMPTY))
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(attempts, 3);
    }

    #[test]
    fn is_orphaned_worktree_ignores_a_git_file_that_is_not_a_gitlink() {
        // A `.git` file that is readable but carries no `gitdir:` pointer is not
        // something we may delete.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), b"not a gitlink\n").unwrap();
        assert!(!is_orphaned_worktree(tmp.path()));
    }

    #[test]
    fn remove_worktree_rejects_a_path_that_is_not_a_worktree() {
        // Neither a repo nor an orphan: refuse it rather than recursively deleting
        // whatever directory was passed in.
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();

        let err = remove_worktree(&plain).unwrap_err();

        assert!(
            err.to_string().contains("not a git worktree"),
            "unexpected error: {err:#}"
        );
        assert!(plain.exists(), "a non-worktree path must be left alone");
    }

    #[test]
    fn remove_worktree_succeeds_while_a_concurrent_writer_winds_down() {
        // Acceptance criterion (#1315): a language server / cargo still writing
        // into `target/` as the window closes makes the recursive rmdir race with
        // "Directory not empty". A background thread reproduces that by
        // repopulating `target/` for a bounded window; removal must retry past it
        // and still succeed once the writer stops.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let target = wt_path.join("target");
        std::fs::create_dir_all(&target).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer_dir = target;
        let writer = std::thread::spawn(move || {
            let mut n = 0u64;
            // Churn hard for ~400ms (well under the ~2.75s retry budget), then
            // stop so a later removal pass finds the directory quiescent.
            let deadline = std::time::Instant::now() + Duration::from_millis(400);
            while !writer_stop.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                let nested = writer_dir.join("nested");
                // Best-effort: the parent may be mid-deletion — ignore failures.
                let _ = std::fs::create_dir_all(&nested);
                let _ = std::fs::write(nested.join(format!("artifact-{n}.tmp")), b"x");
                n += 1;
            }
        });

        let result = remove_worktree(&wt_path);
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        assert!(
            result.is_ok(),
            "removal should retry past the writer: {result:?}"
        );
        assert!(!wt_path.exists(), "the worktree directory should be gone");
    }

    #[tokio::test]
    async fn close_safety_check_flags_untracked_and_does_not_remove_without_confirmation() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        // An untracked file in the worktree would be lost on removal.
        std::fs::write(wt_path.join("scratch.txt"), b"work in progress").unwrap();

        let svc = WorktreesService::new();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        let risks = report.get("risks").and_then(Value::as_array).unwrap();
        assert!(
            risks
                .iter()
                .any(|r| r.get("kind").and_then(Value::as_str) == Some("untracked")),
            "expected an untracked risk: {report}"
        );
        // Still removable — the risk only means "confirm first", not "refuse".
        assert_eq!(report.get("removable").and_then(Value::as_bool), Some(true));
        // The unconfirmed check has no side effects.
        assert!(wt_path.exists());
    }

    #[tokio::test]
    async fn close_confirmed_removes_a_dirty_worktree() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        std::fs::write(wt_path.join("scratch.txt"), b"discard me").unwrap();
        let svc = WorktreesService::new();
        // With confirmation, the risks are overridden and removal proceeds.
        let reply = svc
            .handle(
                "close",
                json!({ "path": wt_path, "remove": true, "confirmed": true }),
            )
            .await
            .unwrap();
        assert_eq!(reply, json!({ "removed": true }));
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn close_refuses_to_remove_the_main_working_tree() {
        let (main, _wtp, _wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        // Phase 1: the main tree reports not-removable, marked main.
        let report = svc
            .handle("close", json!({ "path": main.path(), "remove": true }))
            .await
            .unwrap();
        assert_eq!(report.get("is_main").and_then(Value::as_bool), Some(true));
        assert_eq!(
            report.get("removable").and_then(Value::as_bool),
            Some(false)
        );
        // Phase 2: even a confirmed delete of the main tree is refused
        // defensively, and the directory is untouched.
        assert!(svc
            .handle(
                "close",
                json!({ "path": main.path(), "remove": true, "confirmed": true }),
            )
            .await
            .is_err());
        assert!(main.path().exists());
    }

    #[tokio::test]
    async fn close_removes_a_linked_worktree_on_the_default_branch_and_keeps_the_branch() {
        // The case a naive impl would wrongly protect: a linked worktree checked
        // out on `main` (the default branch) is *still a linked worktree*, so it
        // is fully deletable — and `main` survives (removal never deletes a branch).
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/trunk"), &[], "A");
        repo.set_head("refs/heads/trunk").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("main-wt");
        add_worktree(&repo, a, &wt_path, "main");

        let svc = WorktreesService::new();
        let reply = svc
            .handle(
                "close",
                json!({ "path": wt_path, "remove": true, "confirmed": true }),
            )
            .await
            .unwrap();
        assert_eq!(reply, json!({ "removed": true }));
        assert!(!wt_path.exists());
        // The `main` branch is untouched by the worktree removal.
        assert!(
            repo.find_branch("main", git2::BranchType::Local).is_ok(),
            "the default branch must survive worktree removal"
        );
    }

    #[tokio::test]
    async fn close_is_idempotent_when_the_worktree_is_already_gone() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        // First removal succeeds.
        svc.handle(
            "close",
            json!({ "path": wt_path, "remove": true, "confirmed": true }),
        )
        .await
        .unwrap();
        // A second confirmed close of the now-missing path is a clean success,
        // not an error (a stale snapshot must not crash).
        let reply = svc
            .handle(
                "close",
                json!({ "path": wt_path, "remove": true, "confirmed": true }),
            )
            .await
            .unwrap();
        assert_eq!(reply, json!({ "removed": true }));
    }

    #[tokio::test]
    async fn close_safety_check_detects_detached_head_unreachable_commits() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        // In the worktree, commit onto a detached HEAD so the new commit is
        // reachable from no ref — it would be GC'd on removal.
        let wt_repo = Repository::open(&wt_path).unwrap();
        let parent_oid = wt_repo.head().unwrap().target().unwrap();
        let parent = wt_repo.find_commit(parent_oid).unwrap();
        let orphan = empty_commit(&wt_repo, None, &[&parent], "orphan");
        wt_repo.set_head_detached(orphan).unwrap();

        let svc = WorktreesService::new();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        let risks = report.get("risks").and_then(Value::as_array).unwrap();
        assert!(
            risks
                .iter()
                .any(|r| r.get("kind").and_then(Value::as_str) == Some("unreachable-commits")),
            "expected an unreachable-commits risk: {report}"
        );
    }

    #[tokio::test]
    async fn close_self_close_removes_when_the_requester_owns_the_target() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        // The requesting window itself has the worktree open: it is the only
        // owning window, so there is nothing to wait on — remove and reply, and
        // the extension closes its own window on `ok`.
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [wt_path], "repo": "feature-wt" }),
        )
        .await
        .unwrap();
        let reply = svc
            .handle(
                "close",
                json!({
                    "path": wt_path,
                    "remove": true,
                    "confirmed": true,
                    "requester_key": "w1",
                }),
            )
            .await
            .unwrap();
        assert_eq!(reply, json!({ "removed": true }));
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn close_safety_check_surfaces_the_owning_window() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        // A multi-root window owns the target: the report surfaces its key and
        // folder count so the extension can warn "all N folders will close".
        svc.handle(
            "register",
            json!({ "key": "w2", "folders": [&wt_path, "/tmp/other"], "repo": "feature-wt" }),
        )
        .await
        .unwrap();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        assert_eq!(report.get("open").and_then(Value::as_bool), Some(true));
        assert_eq!(report.get("window_key").and_then(Value::as_str), Some("w2"));
        assert_eq!(
            report.get("window_folder_count").and_then(Value::as_u64),
            Some(2)
        );
    }

    #[tokio::test]
    async fn heartbeat_op_surfaces_a_pending_close_directive_once() {
        let svc = WorktreesService::new();
        svc.handle("register", register_payload("w1", Some("r"), "/tmp/a"))
            .await
            .unwrap();
        // No directive → a plain `{ known: true }`, byte-identical to before.
        assert_eq!(
            svc.handle("heartbeat", json!({ "key": "w1" }))
                .await
                .unwrap(),
            json!({ "known": true })
        );
        // Marked → the next heartbeat carries `close: true`, exactly once.
        svc.registry.mark_close_pending("w1");
        assert_eq!(
            svc.handle("heartbeat", json!({ "key": "w1" }))
                .await
                .unwrap(),
            json!({ "known": true, "close": true })
        );
        assert_eq!(
            svc.handle("heartbeat", json!({ "key": "w1" }))
                .await
                .unwrap(),
            json!({ "known": true })
        );
    }

    #[tokio::test]
    async fn close_signals_a_cross_window_target_then_removes_after_it_closes() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let svc = Arc::new(WorktreesService::new());
        // A *different* window (not the requester) owns the target.
        svc.handle(
            "register",
            json!({ "key": "w2", "folders": [&wt_path], "repo": "feature-wt" }),
        )
        .await
        .unwrap();

        // Drive the destructive close concurrently: it marks w2 to close and
        // waits for it to unregister before removing.
        let svc2 = svc.clone();
        let path = wt_path.clone();
        let close = tokio::spawn(async move {
            svc2.handle(
                "close",
                json!({
                    "path": path,
                    "remove": true,
                    "confirmed": true,
                    "requester_key": "w1",
                }),
            )
            .await
        });

        // Simulate w2's extension: its next heartbeat sees `close: true`, so it
        // closes its window and unregisters. Poll until the directive appears.
        let mut saw_close = false;
        for _ in 0..200 {
            let hb = svc
                .handle("heartbeat", json!({ "key": "w2" }))
                .await
                .unwrap();
            if hb.get("close").and_then(Value::as_bool) == Some(true) {
                saw_close = true;
                svc.handle("unregister", json!({ "key": "w2" }))
                    .await
                    .unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(saw_close, "w2 should have been told to close");

        // Once w2 has unregistered, the close op removes the worktree.
        let reply = close.await.unwrap().unwrap();
        assert_eq!(reply, json!({ "removed": true }));
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn await_windows_closed_times_out_when_a_window_never_closes() {
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w2", "folders": [&wt_path], "repo": "feature-wt" }),
        )
        .await
        .unwrap();
        // The owning window never unregisters: the wait gives up (with a short
        // timeout here) rather than block, and names the still-open window.
        let err = await_windows_closed(
            &svc.registry,
            &wt_path,
            Some("w1"),
            Duration::from_millis(150),
            Duration::from_millis(25),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("w2"),
            "error names the window: {err}"
        );
        // The requester itself is excluded, so a self-only owner returns at once.
        await_windows_closed(
            &svc.registry,
            &wt_path,
            Some("w2"),
            Duration::from_millis(150),
            Duration::from_millis(25),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn close_window_without_remove_replies_closed_and_never_deletes() {
        let (main, _wtp, _wt_path) = repo_with_linked_worktree();
        let svc = WorktreesService::new();
        // "Close Window" on the main tree: no git inspection, no removal.
        let reply = svc
            .handle("close", json!({ "path": main.path(), "remove": false }))
            .await
            .unwrap();
        assert_eq!(reply, json!({ "closed": true }));
        assert!(main.path().exists());
    }

    #[tokio::test]
    async fn close_safety_check_flags_modified_tracked_files() {
        // A tracked file, checked out into the linked worktree, then modified —
        // its content is lost on removal, so it is a `dirty` risk (distinct from
        // the untracked case).
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = commit_file(&repo, "refs/heads/trunk", "tracked.txt", b"original\n", "A");
        repo.set_head("refs/heads/trunk").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");
        std::fs::write(wt_path.join("tracked.txt"), b"uncommitted change\n").unwrap();

        let svc = WorktreesService::new();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        let risks = report.get("risks").and_then(Value::as_array).unwrap();
        assert!(
            risks
                .iter()
                .any(|r| r.get("kind").and_then(Value::as_str) == Some("dirty")),
            "expected a dirty risk: {report}"
        );
    }

    #[tokio::test]
    async fn close_safety_check_flags_an_in_progress_operation() {
        // Plant a MERGE_HEAD in the worktree's gitdir so `repo.state()` reports a
        // non-Clean (interrupted merge) state — its progress is lost on removal.
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let wt_repo = Repository::open(&wt_path).unwrap();
        let head = wt_repo.head().unwrap().target().unwrap();
        std::fs::write(wt_repo.path().join("MERGE_HEAD"), format!("{head}\n")).unwrap();
        assert_ne!(wt_repo.state(), RepositoryState::Clean);

        let svc = WorktreesService::new();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        let risks = report.get("risks").and_then(Value::as_array).unwrap();
        assert!(
            risks
                .iter()
                .any(|r| r.get("kind").and_then(Value::as_str) == Some("in-progress")),
            "expected an in-progress risk: {report}"
        );
    }

    #[tokio::test]
    async fn close_safety_check_reports_unpushed_commits_as_info_not_a_risk() {
        // A linked worktree on `feature`, which tracks `origin/feature` and is one
        // commit ahead. The unpushed commit is INFO (the branch — and thus the
        // commit — survives removal), never a blocking risk.
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/trunk"), &[], "A");
        repo.set_head("refs/heads/trunk").unwrap();
        let a_commit = repo.find_commit(a).unwrap();
        repo.branch("feature", &a_commit, false).unwrap();
        repo.reference("refs/remotes/origin/feature", a, true, "origin feature")
            .unwrap();
        // `feature` advances one commit past `origin/feature`.
        empty_commit(&repo, Some("refs/heads/feature"), &[&a_commit], "B");
        drop(a_commit);
        let mut cfg = repo.config().unwrap();
        cfg.set_str("remote.origin.url", "https://example.invalid/x.git")
            .unwrap();
        cfg.set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
            .unwrap();
        cfg.set_str("branch.feature.remote", "origin").unwrap();
        cfg.set_str("branch.feature.merge", "refs/heads/feature")
            .unwrap();
        // A worktree on the existing `feature` branch (not created fresh, so it
        // keeps the ahead-of-upstream divergence).
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        let reference = repo.find_reference("refs/heads/feature").unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree("feature", &wt_path, Some(&opts)).unwrap();

        let svc = WorktreesService::new();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        // Unpushed commits appear as `info`, and the worktree is still cleanly
        // removable with no blocking risks.
        let info = report.get("info").and_then(Value::as_array).unwrap();
        assert!(
            info.iter()
                .any(|r| r.get("kind").and_then(Value::as_str) == Some("unpushed")),
            "expected an unpushed info note: {report}"
        );
        assert!(
            report
                .get("risks")
                .and_then(Value::as_array)
                .unwrap()
                .is_empty(),
            "unpushed commits alone must not block: {report}"
        );
        assert_eq!(report.get("removable").and_then(Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn close_safety_check_ignores_gitignored_files() {
        // With `.gitignore` committed, an ignored artifact is the only worktree
        // change — it must not count as untracked (it is regenerable), so the
        // worktree stays cleanly removable with no risks.
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = commit_file(&repo, "refs/heads/trunk", ".gitignore", b"build/\n", "A");
        repo.set_head("refs/heads/trunk").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");
        std::fs::create_dir(wt_path.join("build")).unwrap();
        std::fs::write(wt_path.join("build/artifact.o"), b"junk").unwrap();

        let svc = WorktreesService::new();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        assert!(
            report
                .get("risks")
                .and_then(Value::as_array)
                .unwrap()
                .is_empty(),
            "a gitignored file must not create a risk: {report}"
        );
        assert_eq!(report.get("removable").and_then(Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn close_safety_check_treats_a_missing_path_as_already_removed() {
        // The phase-1 check on a path that no longer exists reports it removable
        // with no risks (so the idempotent execute proceeds with no dialog).
        let svc = WorktreesService::new();
        let report = svc
            .handle(
                "close",
                json!({ "path": "/no/such/worktree/xyzzy", "remove": true }),
            )
            .await
            .unwrap();
        assert_eq!(report.get("removable").and_then(Value::as_bool), Some(true));
        assert_eq!(report.get("is_main").and_then(Value::as_bool), Some(false));
        assert!(report
            .get("risks")
            .and_then(Value::as_array)
            .unwrap()
            .is_empty());
        let info = report.get("info").and_then(Value::as_array).unwrap();
        assert!(info
            .iter()
            .any(|r| r.get("kind").and_then(Value::as_str) == Some("already-removed")));
    }

    #[tokio::test]
    async fn close_refuses_a_locked_worktree() {
        // A locked worktree (git worktree lock) must be refused, not forced past
        // (failure mode #6), and left on disk.
        let (main, _wtp, wt_path) = repo_with_linked_worktree();
        let main_repo = Repository::open(main.path()).unwrap();
        main_repo
            .find_worktree("feature")
            .unwrap()
            .lock(Some("under test"))
            .unwrap();

        let svc = WorktreesService::new();
        let err = svc
            .handle(
                "close",
                json!({ "path": wt_path, "remove": true, "confirmed": true }),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("locked"),
            "expected a locked error: {err}"
        );
        assert!(wt_path.exists(), "a locked worktree must not be removed");
    }

    #[tokio::test]
    async fn close_safety_check_does_not_flag_a_detached_head_reachable_from_a_branch() {
        // A detached HEAD that still sits on a commit a branch points to loses
        // nothing on removal, so it must NOT produce an unreachable-commits risk
        // (the false-positive the reachability walk guards against).
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let wt_repo = Repository::open(&wt_path).unwrap();
        // The worktree is on `feature`; detach HEAD onto its current tip, which
        // the `feature` branch still references.
        let tip = wt_repo.head().unwrap().target().unwrap();
        wt_repo.set_head_detached(tip).unwrap();
        assert!(wt_repo.head_detached().unwrap());

        let svc = WorktreesService::new();
        let report = svc
            .handle("close", json!({ "path": wt_path, "remove": true }))
            .await
            .unwrap();
        let risks = report.get("risks").and_then(Value::as_array).unwrap();
        assert!(
            !risks
                .iter()
                .any(|r| r.get("kind").and_then(Value::as_str) == Some("unreachable-commits")),
            "a detached HEAD reachable from a branch must not be flagged: {report}"
        );
    }

    #[test]
    fn worktree_name_for_path_resolves_a_real_worktree_and_errors_otherwise() {
        let (main, _wtp, wt_path) = repo_with_linked_worktree();
        let main_repo = Repository::open(main.path()).unwrap();
        // The real linked worktree resolves to its registered name.
        assert_eq!(
            worktree_name_for_path(&main_repo, &canonical(&wt_path)).unwrap(),
            "feature"
        );
        // A path that is not one of this repo's worktrees is the defensive
        // "not registered" error (the guard behind removal).
        let err =
            worktree_name_for_path(&main_repo, Path::new("/no/such/worktree/xyzzy")).unwrap_err();
        assert!(
            err.to_string().contains("not registered"),
            "expected a not-registered error: {err}"
        );
    }

    #[test]
    fn count_dirty_untracked_degrades_to_zero_on_an_unreadable_index() {
        // A corrupt index makes `statuses()` fail; the count degrades to (0, 0)
        // rather than sinking the whole safety check.
        let (_main, _wtp, wt_path) = repo_with_linked_worktree();
        let repo = Repository::open(&wt_path).unwrap();
        std::fs::write(repo.path().join("index"), b"not a valid git index").unwrap();
        // Confirm the corruption actually breaks status enumeration, so the
        // count is exercising the error-degradation branch (not an empty repo).
        assert!(
            repo.statuses(Some(&mut StatusOptions::new())).is_err(),
            "a corrupt index should make statuses() fail"
        );
        assert_eq!(count_dirty_untracked(&repo), (0, 0));
    }
}
