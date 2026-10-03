//! Bounded, coalesced ahead/behind computation for the worktrees service (#2111).
//!
//! The `ahead-behind` op is the one worktrees op that fans out without limit:
//! every VS Code window re-asks for every worktree of every expanded repo on each
//! pushed delta, so N windows send N identical batches within moments of each
//! other, each of which used to open its own `Repository` per worktree (twice)
//! and walk the commit graph. Under load those handlers queue behind one
//! another, each holding an accepted control-socket connection — and a
//! descriptor — until its turn. Three mechanisms cut that down, none of which
//! can serve a stale answer:
//!
//! - [`WalkMemo`] remembers `graph_ahead_behind` results by commit ids. In a
//!   complete repository ancestry is a pure function of the ids, so a hit is exact,
//!   and a commit, fetch or push changes an id and so misses by construction — no
//!   TTL to tune. A shallow clone is the exception (deepening it changes the answer
//!   without changing any id), so it bypasses the memo — and, because libgit2
//!   applies the shallow cut only through a handle opened at the common dir, its
//!   walks use one (#2147).
//! - [`AheadBehindCoordinator`] makes concurrent requests for one worktree share
//!   one computation (single-flight) while it is still waiting its turn, so a
//!   burst of windows asking at once costs one per worktree, and a request never
//!   gets an answer older than itself.
//! - It also caps how many computations run at once, bounding blocking-pool use
//!   and the number of live `Repository` objects.
//!
//! What a computation reads the repository *through* is [`super::shared_repo`]'s
//! business (#2121): the coordinator owns the [`RepoPool`] those handles come from,
//! sized to the same cap, so a batch over N linked worktrees of one repo opens it
//! at most that many times instead of N.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use git2::{Oid, Repository};
use tokio::sync::{OnceCell, Semaphore};

use super::shared_repo::RepoPool;

/// How many worktrees may be walked at once. Enough to keep a multi-core machine
/// busy, few enough that the walks cannot saturate the blocking pool or pin more
/// than a handful of repositories' pack files open together.
const MAX_CONCURRENT_COMPUTATIONS: usize = 4;

/// How many walk results the process-wide memo keeps before it starts over. An
/// entry is a path and two ids — about 100 bytes — so this is a ceiling on
/// memory measured in hundreds of kilobytes, not a working-set tuning knob: a
/// real working set is one entry per (worktree, tip), a few dozen at a time.
const WALK_MEMO_CAPACITY: usize = 4096;

/// One worktree's divergence, as the `ahead-behind` op reports it. Each part is
/// independently absent, exactly as on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Divergence {
    /// `(ahead, behind)` against the branch's own upstream.
    pub(super) ahead_behind: Option<(usize, usize)>,
    /// Commits behind the repository's remote default branch (#1457).
    pub(super) main_behind: Option<usize>,
    /// Whether the repository is shallow, so the counts above depend on how deep
    /// the clone currently is — which no commit id records (#2120). Reported on the
    /// wire so a client that memoizes by ids can see that this is the one case where
    /// equal ids do not mean an equal answer, for the same reason [`WalkMemo`]
    /// bypasses its own memo here.
    pub(super) shallow: bool,
}

/// The identity of one graph walk: the repository's common dir plus both tips.
/// Equal ids mean equal history in a complete clone, so the common dir only keeps
/// unrelated repositories from sharing entries. (Grafts and `replace` refs can in
/// principle rewrite ancestry behind an unchanged id; they are not used on the
/// repositories this daemon watches, and the memo turns over at its capacity.)
type WalkKey = (PathBuf, Oid, Oid);

/// A bounded memo of `graph_ahead_behind` results, keyed by [`WalkKey`].
struct WalkMemo {
    capacity: usize,
    entries: Mutex<HashMap<WalkKey, (usize, usize)>>,
}

impl WalkMemo {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The memoized counts for `key`, else the result of `walk`, which is
    /// remembered when it succeeds. The lock is not held across `walk`, so two
    /// racing misses may both walk — a harmless duplicate, never a wrong answer.
    fn get_or_walk(
        &self,
        key: WalkKey,
        walk: impl FnOnce() -> Option<(usize, usize)>,
    ) -> Option<(usize, usize)> {
        if let Some(hit) = self.lock().get(&key) {
            return Some(*hit);
        }
        let counts = walk()?;
        let mut entries = self.lock();
        if entries.len() >= self.capacity {
            // Start over rather than tracking recency: the entries are cheap to
            // rebuild and the cap is far above a real working set.
            entries.clear();
        }
        entries.insert(key, counts);
        Some(counts)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<WalkKey, (usize, usize)>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

/// The process-wide walk memo. Global because the callers are free functions with
/// no service handle (the tray menu and `list` reach it through `git_status`), and
/// sound as a global because its key fully determines the value.
static WALKS: LazyLock<WalkMemo> = LazyLock::new(|| WalkMemo::with_capacity(WALK_MEMO_CAPACITY));

/// Commits `local` is ahead of and behind `upstream` in `repo`, memoized by id.
///
/// `None` when the walk fails (an id with no commit behind it); failures are not
/// memoized, so a later call retries. A shallow repository is never memoized: its
/// answer depends on how deep it currently is, which no id records. It is also
/// walked through a handle that applies the cut (see [`walk_shallow`]).
pub(super) fn graph_ahead_behind(
    repo: &Repository,
    local: Oid,
    upstream: Oid,
) -> Option<(usize, usize)> {
    graph_ahead_behind_in(&WALKS, repo, local, upstream)
}

/// [`graph_ahead_behind`] against an explicit memo, so tests need not share the
/// process-wide one.
fn graph_ahead_behind_in(
    memo: &WalkMemo,
    repo: &Repository,
    local: Oid,
    upstream: Oid,
) -> Option<(usize, usize)> {
    if is_shallow(repo) {
        return walk_shallow(repo, local, upstream);
    }
    let key = (repo.commondir().to_path_buf(), local, upstream);
    memo.get_or_walk(key, || repo.graph_ahead_behind(local, upstream).ok())
}

/// Whether `repo` is shallow, as git sees it: its common dir holds a non-empty
/// `shallow` file.
///
/// `Repository::is_shallow` is not that. libgit2 looks for the marker in the
/// handle's own gitdir, which for a repository opened at a linked worktree is
/// `<commondir>/worktrees/<name>`, while git keeps `shallow` in the common dir that
/// every worktree shares. So that handle reports a shallow clone as complete
/// (#2147). The two agree for a main checkout, whose gitdir is the common dir.
/// The marker rule is libgit2's — a non-empty file — applied to the common dir.
pub(super) fn is_shallow(repo: &Repository) -> bool {
    repo.is_shallow()
        || std::fs::metadata(repo.commondir().join("shallow"))
            .is_ok_and(|marker| marker.is_file() && marker.len() > 0)
}

/// The counts for a shallow `repo`, which must be walked through a handle that
/// applies the cut.
///
/// libgit2 reads the shallow grafts from the gitdir of the handle it opens, so a
/// handle rooted at a linked worktree walks straight into the parents the clone
/// does not have: it fails with "object not found", and the row would lose its
/// counts (#2147). A handle opened at the common dir reads the marker, and so
/// does what git does. A failed open is `None` — no counts — rather than counts
/// from the uncut history, and is logged at `debug` (not `warn`: a shallow row is
/// never cached by clients, so a persistent failure would repeat on every ask).
fn walk_shallow(repo: &Repository, local: Oid, upstream: Oid) -> Option<(usize, usize)> {
    if !repo.is_worktree() {
        return repo.graph_ahead_behind(local, upstream).ok();
    }
    match Repository::open(repo.commondir()) {
        Ok(common) => common.graph_ahead_behind(local, upstream).ok(),
        Err(e) => {
            tracing::debug!(
                "cannot open {} to walk a shallow repository: {e}",
                repo.commondir().display()
            );
            None
        }
    }
}

/// One computation for one worktree, shared by every request that joins it.
#[derive(Default)]
struct Flight {
    result: OnceCell<Divergence>,
    /// Set the moment the computation begins reading the repository. A request
    /// that arrives after this must not join it: its answer would describe the
    /// repository as it was before the request was made.
    started: AtomicBool,
}

/// Runs divergence computations so that concurrent requests for the same
/// worktree share one, and so that at most a fixed number run at a time.
///
/// A request joins a computation only while it is still **waiting for its turn**
/// (queued behind the cap, or not yet on its thread). That is exactly when sharing
/// is worth the most — under load, with the cap saturated and the queue deep — and
/// it keeps every answer exact: a computation that has begun reading the
/// repository is never joined by a later request, which starts its own instead.
pub(super) struct AheadBehindCoordinator {
    /// The newest computation per worktree. An entry exists only while a request
    /// is being served; it is removed when its last interested caller finishes, so
    /// nothing here is a cache.
    in_flight: Mutex<HashMap<PathBuf, Arc<Flight>>>,
    permits: Arc<Semaphore>,
    /// How many computations run at once, which is also the most repositories the
    /// shared pool keeps open.
    limit: usize,
    /// The pool of shared repository handles, held weakly so it exists only while
    /// some request holds it (see [`Self::pool`]).
    pool: Mutex<Weak<RepoPool>>,
}

impl AheadBehindCoordinator {
    pub(super) fn new() -> Self {
        Self::with_concurrency(MAX_CONCURRENT_COMPUTATIONS)
    }

    pub(super) fn with_concurrency(limit: usize) -> Self {
        Self {
            in_flight: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(limit)),
            limit,
            pool: Mutex::new(Weak::new()),
        }
    }

    /// The pool of repository handles that computations share (#2121).
    ///
    /// Callers hold the returned `Arc` for as long as they have work outstanding
    /// and move a clone into each computation. Concurrent callers get the **same**
    /// pool, so the handle cap holds across overlapping batches; once the last
    /// holder drops it, every handle is closed and the next call starts a fresh
    /// pool. That makes the handles live exactly as long as the requests they
    /// serve — never a daemon-lifetime cache, which could otherwise age into a
    /// stale view of a repository.
    pub(super) fn pool(&self) -> Arc<RepoPool> {
        let mut weak = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pool) = weak.upgrade() {
            return pool;
        }
        let pool = Arc::new(RepoPool::new(self.limit));
        *weak = Arc::downgrade(&pool);
        pool
    }

    /// The divergence of the worktree at `path`, from a computation that has not
    /// begun yet if one is queued for it, else from `compute` run on a blocking
    /// thread.
    ///
    /// `compute` is only called when this caller is the one to run the work; a
    /// caller that joins a queued computation drops it unused.
    pub(super) async fn get_or_compute<F>(&self, path: PathBuf, compute: F) -> Divergence
    where
        F: FnOnce(PathBuf) -> Divergence + Send + 'static,
    {
        let flight = self.join_or_start(&path);
        // Removes the entry on the way out, including when this future is dropped
        // mid-flight, so an abandoned request cannot leave a path stuck.
        let _forget = Forget {
            in_flight: &self.in_flight,
            path: &path,
            flight: &flight,
        };
        *flight
            .result
            .get_or_init(|| self.compute_bounded(Arc::clone(&flight), path.clone(), compute))
            .await
    }

    /// The computation queued for `path`, or a fresh one registered in its place.
    fn join_or_start(&self, path: &PathBuf) -> Arc<Flight> {
        let mut in_flight = self.lock();
        if let Some(queued) = in_flight.get(path) {
            if !queued.started.load(Ordering::SeqCst) {
                return Arc::clone(queued);
            }
        }
        let fresh = Arc::new(Flight::default());
        in_flight.insert(path.clone(), Arc::clone(&fresh));
        fresh
    }

    /// Waits for a permit, then runs `compute` on a blocking thread holding it.
    async fn compute_bounded<F>(&self, flight: Arc<Flight>, path: PathBuf, compute: F) -> Divergence
    where
        F: FnOnce(PathBuf) -> Divergence + Send + 'static,
    {
        let Ok(permit) = Arc::clone(&self.permits).acquire_owned().await else {
            // Unreachable while `self` lives: the semaphore is never closed.
            return Divergence::default();
        };
        // The permit moves into the closure rather than living in this future.
        // A caller that gives up cannot stop the blocking thread, so releasing the
        // permit with the future would let more than the cap run at once.
        let joined = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // Marked before any read, so a request that sees `started == false`
            // is guaranteed this computation reads the repository after it arrived.
            flight.started.store(true, Ordering::SeqCst);
            compute(path)
        })
        .await;
        joined.unwrap_or_else(|e| {
            tracing::warn!("ahead/behind computation failed: {e}");
            Divergence::default()
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Arc<Flight>>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn in_flight_len(&self) -> usize {
        self.lock().len()
    }
}

/// Drop guard that removes a path's in-flight entry, if it is still the same one.
struct Forget<'a> {
    in_flight: &'a Mutex<HashMap<PathBuf, Arc<Flight>>>,
    path: &'a PathBuf,
    flight: &'a Arc<Flight>,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A later request may already have replaced the entry; leave that one.
        if in_flight
            .get(self.path)
            .is_some_and(|current| Arc::ptr_eq(current, self.flight))
        {
            in_flight.remove(self.path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use super::*;

    fn divergence(ahead: usize) -> Divergence {
        Divergence {
            ahead_behind: Some((ahead, 0)),
            ..Divergence::default()
        }
    }

    /// Waits (briefly, politely) until `flag` is set.
    async fn wait_until(flag: &AtomicBool) {
        for _ in 0..400 {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for a computation to start"); // omni-dev: coverage ignore-line reason="only runs if a started computation never sets its flag within 2s, which fails the calling test; a passing run never takes it"
    }

    /// Starts a computation for `path` that holds a permit for `hold`, and returns
    /// once its thread is running, so a test can build a queue behind it.
    async fn hold_a_permit(
        coordinator: &Arc<AheadBehindCoordinator>,
        path: &str,
        hold: Duration,
    ) -> tokio::task::JoinHandle<Divergence> {
        let running = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&running);
        let coordinator = Arc::clone(coordinator);
        let path = PathBuf::from(path);
        let task = tokio::spawn(async move {
            coordinator
                .get_or_compute(path, move |_| {
                    flag.store(true, Ordering::SeqCst);
                    std::thread::sleep(hold);
                    divergence(0)
                })
                .await
        });
        wait_until(&running).await;
        task
    }

    /// While the cap is saturated, a queue forms behind it — and everyone queued for
    /// one worktree shares a single computation, however many ask.
    #[tokio::test]
    async fn requests_queued_for_one_path_share_one_computation() {
        let coordinator = Arc::new(AheadBehindCoordinator::with_concurrency(1));
        let blocker = hold_a_permit(&coordinator, "/blocker", Duration::from_millis(150)).await;
        let calls = Arc::new(AtomicUsize::new(0));

        let results = futures::future::join_all((0..8).map(|_| {
            let calls = Arc::clone(&calls);
            coordinator.get_or_compute(PathBuf::from("/repo/a"), move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                divergence(7)
            })
        }))
        .await;
        blocker.await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "work was duplicated");
        assert!(results.iter().all(|r| *r == divergence(7)));
        assert_eq!(coordinator.in_flight_len(), 0, "an entry leaked");
    }

    /// The exactness guarantee: once a computation has begun reading the repository
    /// a later request does not join it — the repository may have changed since — and
    /// gets an answer of its own.
    #[tokio::test]
    async fn a_request_never_joins_a_computation_that_has_already_started() {
        let coordinator = Arc::new(AheadBehindCoordinator::with_concurrency(4));
        let leader = hold_a_permit(&coordinator, "/repo/a", Duration::from_millis(150)).await;

        let later = coordinator
            .get_or_compute(PathBuf::from("/repo/a"), |_| divergence(2))
            .await;

        assert_eq!(later, divergence(2), "the later request got a stale answer");
        assert_eq!(leader.await.unwrap(), divergence(0));
        assert_eq!(coordinator.in_flight_len(), 0, "an entry leaked");
    }

    #[tokio::test]
    async fn distinct_paths_are_computed_independently() {
        let coordinator = AheadBehindCoordinator::new();
        let calls = Arc::new(AtomicUsize::new(0));

        futures::future::join_all(["/a", "/b", "/c"].map(|p| {
            let calls = Arc::clone(&calls);
            coordinator.get_or_compute(PathBuf::from(p), move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                divergence(7)
            })
        }))
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Nothing is cached once a computation finishes: the next request sees the
    /// repository as it is then, never an earlier answer.
    #[tokio::test]
    async fn a_later_request_recomputes_instead_of_reusing_a_finished_result() {
        let coordinator = AheadBehindCoordinator::new();
        let path = PathBuf::from("/repo/a");

        let first = coordinator
            .get_or_compute(path.clone(), |_| divergence(1))
            .await;
        let second = coordinator.get_or_compute(path, |_| divergence(2)).await;

        assert_eq!(first, divergence(1));
        assert_eq!(second, divergence(2));
    }

    #[tokio::test]
    async fn no_more_than_the_cap_run_at_once() {
        let coordinator = AheadBehindCoordinator::with_concurrency(2);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        futures::future::join_all((0..8).map(|i| {
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            coordinator.get_or_compute(PathBuf::from(format!("/repo/{i}")), move |_| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                running.fetch_sub(1, Ordering::SeqCst);
                divergence(0)
            })
        }))
        .await;

        let peak = peak.load(Ordering::SeqCst);
        assert!(peak <= 2, "{peak} computations ran at once, cap is 2");
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }

    /// A caller that gives up while others are queued with it must not strand them:
    /// one of the waiters takes over and runs its own computation.
    #[tokio::test]
    async fn a_waiter_takes_over_when_the_queued_caller_is_dropped() {
        let coordinator = Arc::new(AheadBehindCoordinator::with_concurrency(1));
        let started = Instant::now();
        let blocker = hold_a_permit(&coordinator, "/blocker", Duration::from_millis(150)).await;

        // Both queue for "/repo/a" behind the blocker, sharing one flight. The
        // waiter joins 30ms after the leader and the leader is abandoned at 60ms —
        // dropped by the timeout, mid-wait, exactly as a cancelled request would be.
        let waiter = {
            let coordinator = Arc::clone(&coordinator);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                coordinator
                    .get_or_compute(PathBuf::from("/repo/a"), |_| divergence(2))
                    .await
            })
        };
        let abandoned = tokio::time::timeout(
            Duration::from_millis(60),
            coordinator.get_or_compute(PathBuf::from("/repo/a"), |_| divergence(1)),
        )
        .await;
        assert!(abandoned.is_err(), "the leader should still be queued");

        let result = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter must not hang")
            .unwrap();
        assert_eq!(result, divergence(2), "the waiter ran its own computation");
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the waiter ran before the blocker released the only permit"
        );
        blocker.await.unwrap();
        assert_eq!(coordinator.in_flight_len(), 0, "an entry leaked");
    }

    /// A caller that gives up cannot stop its blocking thread, so the permit must
    /// stay held until that thread finishes — or the cap would not bound anything.
    #[tokio::test]
    async fn a_dropped_caller_does_not_free_the_permit_its_thread_still_holds() {
        let coordinator = Arc::new(AheadBehindCoordinator::with_concurrency(1));
        let started = Instant::now();
        let abandoned = hold_a_permit(&coordinator, "/repo/a", Duration::from_millis(150)).await;
        abandoned.abort();

        let next = coordinator
            .get_or_compute(PathBuf::from("/repo/b"), |_| divergence(2))
            .await;

        assert_eq!(next, divergence(2));
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "a second computation ran while the abandoned one held the only permit"
        );
    }

    /// A closed semaphore is unreachable in production (nothing closes it), but if
    /// it ever happened the caller must get the empty answer, not a panic or a hang.
    #[tokio::test]
    async fn a_closed_semaphore_yields_an_empty_answer_and_leaks_nothing() {
        let coordinator = AheadBehindCoordinator::with_concurrency(1);
        coordinator.permits.close();

        let result = coordinator
            .get_or_compute(PathBuf::from("/repo/a"), |_| divergence(1))
            .await;

        assert_eq!(result, Divergence::default());
        assert_eq!(coordinator.in_flight_len(), 0, "an entry leaked");
    }

    /// A computation that panics degrades to the empty answer — and gives its
    /// permit back, so the cap is not lowered by every failure.
    #[tokio::test]
    async fn a_panicking_computation_yields_an_empty_answer_and_frees_its_permit() {
        let coordinator = AheadBehindCoordinator::with_concurrency(1);

        let failed = coordinator
            .get_or_compute(PathBuf::from("/repo/a"), |_| panic!("boom"))
            .await;
        assert_eq!(failed, Divergence::default());
        assert_eq!(coordinator.in_flight_len(), 0, "an entry leaked");

        let next = tokio::time::timeout(
            Duration::from_secs(5),
            coordinator.get_or_compute(PathBuf::from("/repo/b"), |_| divergence(2)),
        )
        .await
        .expect("the only permit must have been released");
        assert_eq!(next, divergence(2));
    }

    #[test]
    fn walk_memo_runs_the_walk_once_per_key() {
        let memo = WalkMemo::with_capacity(8);
        let walks = AtomicUsize::new(0);
        let key = (PathBuf::from("/r/.git"), Oid::ZERO_SHA1, Oid::ZERO_SHA1);

        for _ in 0..3 {
            let counts = memo.get_or_walk(key.clone(), || {
                walks.fetch_add(1, Ordering::SeqCst);
                Some((2, 3))
            });
            assert_eq!(counts, Some((2, 3)));
        }
        assert_eq!(walks.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn walk_memo_does_not_remember_a_failed_walk() {
        let memo = WalkMemo::with_capacity(8);
        let key = (PathBuf::from("/r/.git"), Oid::ZERO_SHA1, Oid::ZERO_SHA1);

        assert_eq!(memo.get_or_walk(key.clone(), || None), None);
        assert_eq!(memo.len(), 0);
        assert_eq!(memo.get_or_walk(key, || Some((1, 1))), Some((1, 1)));
    }

    #[test]
    fn walk_memo_starts_over_at_capacity_instead_of_growing() {
        let memo = WalkMemo::with_capacity(2);
        let key = |n: u8| {
            (
                PathBuf::from(format!("/r{n}/.git")),
                Oid::ZERO_SHA1,
                Oid::ZERO_SHA1,
            )
        };
        for n in 0..5 {
            memo.get_or_walk(key(n), || Some((usize::from(n), 0)));
            assert!(memo.len() <= 2, "grew to {}", memo.len());
        }
        // The newest entry survives the reset and still answers without walking.
        assert_eq!(memo.get_or_walk(key(4), || None), Some((4, 0)));
    }

    /// The memo must follow the commit graph, not stick: a new commit changes the
    /// tip id, so the same worktree is walked again and reports the new count.
    #[test]
    fn graph_ahead_behind_tracks_new_commits_and_matches_libgit2() {
        let memo = WalkMemo::with_capacity(8);
        let dir = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());

        assert_eq!(
            graph_ahead_behind_in(&memo, &repo, first, base),
            Some((1, 0))
        );
        assert_eq!(
            graph_ahead_behind_in(&memo, &repo, first, base),
            Some((1, 0))
        );
        assert_eq!(memo.len(), 1, "the repeat is a hit, not a second entry");
        // A tip that moved is a different key, so the answer moves with it.
        assert_eq!(
            graph_ahead_behind_in(&memo, &repo, second, base),
            Some((2, 0))
        );
        assert_eq!(
            graph_ahead_behind_in(&memo, &repo, second, base),
            repo.graph_ahead_behind(second, base).ok()
        );
    }

    /// A shallow repository's answer depends on how deep it currently is, which no
    /// commit id records — so deepening it would leave a memoized count wrong.
    #[test]
    fn a_shallow_repository_bypasses_the_memo() {
        let memo = WalkMemo::with_capacity(8);
        let dir = tempfile::tempdir().unwrap();
        let (repo, base, first, _second) = three_commits(dir.path());
        // `.git/shallow` is what makes git (and libgit2) treat the clone as shallow.
        std::fs::write(repo.path().join("shallow"), format!("{base}\n")).unwrap();
        assert!(repo.is_shallow());

        assert!(graph_ahead_behind_in(&memo, &repo, first, base).is_some());
        assert_eq!(memo.len(), 0, "a shallow repository was memoized");
    }

    /// A linked worktree of `repo` at `parent/name`, on a new branch at `base`.
    fn add_linked_worktree(repo: &Repository, base: Oid, parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        repo.branch(name, &repo.find_commit(base).unwrap(), false)
            .unwrap();
        let reference = repo.find_reference(&format!("refs/heads/{name}")).unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree(name, &path, Some(&opts)).unwrap();
        path
    }

    /// Marks `repo` shallow at `tip`, the way a `--depth` clone does: `tip` stays
    /// and its parents are cut away. git keeps the marker in the common dir, which
    /// every worktree of the repository shares.
    fn mark_shallow(repo: &Repository, tip: Oid) {
        std::fs::write(repo.commondir().join("shallow"), format!("{tip}\n")).unwrap();
    }

    /// Deletes a loose object, as a shallow clone never has the commits it cut.
    fn forget_object(repo: &Repository, id: Oid) {
        let hex = id.to_string();
        let path = repo.path().join("objects").join(&hex[..2]).join(&hex[2..]);
        std::fs::remove_file(path).unwrap();
    }

    /// git keeps the `shallow` marker in the common dir, which no linked worktree's
    /// own gitdir contains, and libgit2 looks only in the handle's own gitdir. So
    /// the handle a linked worktree is opened as must not decide shallowness.
    #[test]
    fn a_linked_worktree_of_a_shallow_repository_is_shallow() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, _second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();
        assert!(!is_shallow(&repo) && !is_shallow(&linked));

        mark_shallow(&repo, first);

        assert!(is_shallow(&repo));
        assert!(is_shallow(&linked));

        // An empty marker is not shallow: libgit2's rule for a main checkout, which
        // the common-dir check follows so the two cannot disagree. (git removes the
        // file when it unshallows, so an empty one is not a state git leaves.)
        std::fs::write(repo.commondir().join("shallow"), "").unwrap();
        assert!(!is_shallow(&repo) && !is_shallow(&linked));
    }

    /// The premise of [`is_shallow`] and [`walk_shallow`], pinned the way
    /// `shared_repo`'s `libgit2_reads_shallow_grafts_when_a_repository_is_opened`
    /// pins its own: a handle opened at a linked worktree neither reports the common
    /// dir's marker nor applies its cut, while one at the common dir does both. If
    /// libgit2 starts looking in the common dir this fails, which means the
    /// workaround has become unnecessary, not that it regressed.
    #[test]
    fn libgit2_ignores_the_common_dirs_shallow_marker_from_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let path = add_linked_worktree(&repo, base, wts.path(), "feature");
        mark_shallow(&repo, first);
        forget_object(&repo, base);
        let linked = Repository::open(path).unwrap();
        let common = Repository::open(repo.commondir()).unwrap();

        assert!(!linked.is_shallow());
        assert!(linked.graph_ahead_behind(second, first).is_err());
        assert!(common.is_shallow());
        assert_eq!(common.graph_ahead_behind(second, first).unwrap(), (1, 0));
    }

    #[test]
    fn a_linked_worktree_of_a_shallow_repository_bypasses_the_memo() {
        let memo = WalkMemo::with_capacity(8);
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();
        mark_shallow(&repo, first);

        assert!(graph_ahead_behind_in(&memo, &linked, second, first).is_some());
        assert_eq!(memo.len(), 0, "a shallow repository was memoized");
    }

    /// The walk must see the cut, which only a handle opened at the common dir does:
    /// with `first` shallow it is a root, so `base` is behind it rather than
    /// reachable from it.
    #[test]
    fn a_linked_worktree_of_a_shallow_repository_is_walked_with_the_cut_applied() {
        let memo = WalkMemo::with_capacity(8);
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, _second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();
        mark_shallow(&repo, first);

        let complete = Repository::open(repo.commondir()).unwrap();
        assert!(complete.is_shallow());
        assert_eq!(complete.graph_ahead_behind(first, base).unwrap(), (1, 1));
        assert_eq!(
            graph_ahead_behind_in(&memo, &linked, first, base),
            Some((1, 1))
        );
    }

    /// What a real `--depth` clone does to a linked worktree: the commits behind the
    /// cut are not in the object database, so a walk that does not apply the cut
    /// fails on the missing parent and the row would lose its counts.
    #[test]
    fn a_linked_worktree_of_a_shallow_clone_still_gets_counts_without_the_cut_commits() {
        let memo = WalkMemo::with_capacity(8);
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let path = add_linked_worktree(&repo, base, wts.path(), "feature");
        mark_shallow(&repo, first);
        forget_object(&repo, base);
        let linked = Repository::open(path).unwrap();

        assert_eq!(
            graph_ahead_behind_in(&memo, &linked, second, first),
            Some((1, 0))
        );
    }

    /// A common dir that will not open leaves the row without counts, not with
    /// counts from a handle that ignores the cut.
    #[test]
    fn a_common_dir_that_will_not_open_leaves_a_shallow_worktree_without_counts() {
        let memo = WalkMemo::with_capacity(8);
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();
        mark_shallow(&repo, first);
        // Without `HEAD` the directory is no longer a repository to libgit2.
        std::fs::remove_file(repo.commondir().join("HEAD")).unwrap();
        assert!(Repository::open(repo.commondir()).is_err());

        assert_eq!(graph_ahead_behind_in(&memo, &linked, second, first), None);
        assert_eq!(memo.len(), 0);
    }

    /// base ← first ← second, written into a fresh repository at `dir`.
    fn three_commits(dir: &std::path::Path) -> (Repository, Oid, Oid, Oid) {
        let repo = Repository::init(dir).unwrap();
        let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let base = repo.commit(None, &sig, &sig, "base", &tree, &[]).unwrap();
        let first = {
            let parent = repo.find_commit(base).unwrap();
            repo.commit(None, &sig, &sig, "one", &tree, &[&parent])
                .unwrap()
        };
        let second = {
            let parent = repo.find_commit(first).unwrap();
            repo.commit(None, &sig, &sig, "two", &tree, &[&parent])
                .unwrap()
        };
        drop(tree);
        (repo, base, first, second)
    }

    /// Overlapping requests must share one pool, or the handle cap would be per
    /// request instead of overall; and the pool must not outlive them, or its
    /// handles would age into a stale view of the repository.
    #[test]
    fn overlapping_holders_share_one_pool_and_it_is_released_with_the_last() {
        let coordinator = AheadBehindCoordinator::with_concurrency(3);
        let first = coordinator.pool();
        let second = coordinator.pool();
        assert!(Arc::ptr_eq(&first, &second));

        let weak = Arc::downgrade(&first);
        drop(first);
        assert!(weak.upgrade().is_some(), "the second holder still has it");
        drop(second);
        assert!(weak.upgrade().is_none(), "the last holder released it");

        // The next batch starts a fresh pool rather than finding a dead one.
        let next = coordinator.pool();
        assert_eq!(Arc::strong_count(&next), 1);
    }
}
