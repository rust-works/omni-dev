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
//! - [`WalkMemo`] remembers `graph_ahead_behind` results by commit ids. Ancestry
//!   is a pure function of the ids, so a hit is exact, and a commit, fetch or push
//!   changes an id and so misses by construction — no TTL to tune.
//! - [`AheadBehindCoordinator`] makes concurrent requests for one worktree share
//!   one computation (single-flight), so 26 windows asking at once cost one.
//! - It also caps how many computations run at once, bounding blocking-pool use
//!   and the number of live `Repository` objects (each holds its pack files open).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use git2::{Oid, Repository};
use tokio::sync::{OnceCell, Semaphore};

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
}

/// The identity of one graph walk: the repository's common dir plus both tips.
/// The common dir keeps repositories apart even though equal ids mean equal
/// history in a complete clone — a shallow clone sees the same ids differently.
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
/// memoized, so a later call retries.
pub(super) fn graph_ahead_behind(
    repo: &Repository,
    local: Oid,
    upstream: Oid,
) -> Option<(usize, usize)> {
    let key = (repo.commondir().to_path_buf(), local, upstream);
    WALKS.get_or_walk(key, || repo.graph_ahead_behind(local, upstream).ok())
}

/// Runs divergence computations so that concurrent requests for the same
/// worktree share one, and so that at most a fixed number run at a time.
pub(super) struct AheadBehindCoordinator {
    /// The computation in flight for each worktree. An entry exists only while a
    /// request is being served: it is removed when the last interested caller
    /// finishes, so a later request always computes afresh — nothing here is a
    /// cache, and a result is never older than the request that joined it.
    in_flight: Mutex<HashMap<PathBuf, Arc<OnceCell<Divergence>>>>,
    permits: Arc<Semaphore>,
}

impl AheadBehindCoordinator {
    pub(super) fn new() -> Self {
        Self::with_concurrency(MAX_CONCURRENT_COMPUTATIONS)
    }

    fn with_concurrency(limit: usize) -> Self {
        Self {
            in_flight: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(limit)),
        }
    }

    /// The divergence of the worktree at `path`, from the computation already in
    /// flight for it if there is one, else from `compute` run on a blocking thread.
    ///
    /// `compute` is only called when this caller is the one to start the work; a
    /// caller that joins an existing computation drops it unused.
    pub(super) async fn get_or_compute<F>(&self, path: PathBuf, compute: F) -> Divergence
    where
        F: FnOnce(PathBuf) -> Divergence + Send + 'static,
    {
        let cell = Arc::clone(self.lock().entry(path.clone()).or_default());
        // Removes the entry on the way out, including when this future is dropped
        // mid-flight, so an abandoned request cannot leave a path stuck.
        let _forget = Forget {
            in_flight: &self.in_flight,
            path: &path,
            cell: &cell,
        };
        *cell
            .get_or_init(|| self.compute_bounded(path.clone(), compute))
            .await
    }

    /// Waits for a permit, then runs `compute` on a blocking thread holding it.
    async fn compute_bounded<F>(&self, path: PathBuf, compute: F) -> Divergence
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
            compute(path)
        })
        .await;
        joined.unwrap_or_else(|e| {
            tracing::warn!("ahead/behind computation failed: {e}");
            Divergence::default()
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Arc<OnceCell<Divergence>>>> {
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
    in_flight: &'a Mutex<HashMap<PathBuf, Arc<OnceCell<Divergence>>>>,
    path: &'a PathBuf,
    cell: &'a Arc<OnceCell<Divergence>>,
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
            .is_some_and(|current| Arc::ptr_eq(current, self.cell))
        {
            in_flight.remove(self.path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn divergence(ahead: usize) -> Divergence {
        Divergence {
            ahead_behind: Some((ahead, 0)),
            main_behind: None,
        }
    }

    /// A computation that counts how often it ran and takes long enough for
    /// concurrent callers to pile up behind it.
    fn slow_counting(
        calls: &Arc<AtomicUsize>,
    ) -> impl FnOnce(PathBuf) -> Divergence + Send + 'static {
        let calls = Arc::clone(calls);
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
            divergence(7)
        }
    }

    #[tokio::test]
    async fn concurrent_requests_for_one_path_share_one_computation() {
        let coordinator = AheadBehindCoordinator::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let path = PathBuf::from("/repo/a");

        let results = futures::future::join_all(
            (0..8).map(|_| coordinator.get_or_compute(path.clone(), slow_counting(&calls))),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1, "work was duplicated");
        assert!(results.iter().all(|r| *r == divergence(7)));
        assert_eq!(coordinator.in_flight_len(), 0, "an entry leaked");
    }

    #[tokio::test]
    async fn distinct_paths_are_computed_independently() {
        let coordinator = AheadBehindCoordinator::new();
        let calls = Arc::new(AtomicUsize::new(0));

        futures::future::join_all(
            ["/a", "/b", "/c"]
                .map(|p| coordinator.get_or_compute(PathBuf::from(p), slow_counting(&calls))),
        )
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

    /// A caller that gives up must not strand the others, nor free its permit
    /// while its blocking thread is still working.
    #[tokio::test]
    async fn a_waiter_takes_over_when_the_computing_caller_is_dropped() {
        let coordinator = Arc::new(AheadBehindCoordinator::with_concurrency(1));
        let path = PathBuf::from("/repo/a");
        let started = std::time::Instant::now();

        let leader = {
            let coordinator = Arc::clone(&coordinator);
            let path = path.clone();
            tokio::spawn(async move {
                coordinator
                    .get_or_compute(path, |_| {
                        std::thread::sleep(Duration::from_millis(150));
                        divergence(1)
                    })
                    .await
            })
        };
        // Let the leader take the permit and start its blocking work.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let waiter = {
            let coordinator = Arc::clone(&coordinator);
            let path = path.clone();
            tokio::spawn(async move { coordinator.get_or_compute(path, |_| divergence(2)).await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        leader.abort();

        let result = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter must not hang")
            .unwrap();
        assert_eq!(result, divergence(2), "the waiter ran its own computation");
        // The abandoned leader's blocking thread still held the only permit for its
        // full 150ms, so the waiter could not have started before it finished.
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the waiter ran while the leader's work still held the permit: {:?}",
            started.elapsed()
        );
        assert_eq!(coordinator.in_flight_len(), 0, "an entry leaked");
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
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let base = repo.commit(None, &sig, &sig, "base", &tree, &[]).unwrap();
        let base_commit = repo.find_commit(base).unwrap();
        let first = repo
            .commit(None, &sig, &sig, "one", &tree, &[&base_commit])
            .unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        let second = repo
            .commit(None, &sig, &sig, "two", &tree, &[&first_commit])
            .unwrap();

        assert_eq!(graph_ahead_behind(&repo, first, base), Some((1, 0)));
        assert_eq!(graph_ahead_behind(&repo, first, base), Some((1, 0)));
        // A tip that moved is a different key, so the answer moves with it.
        assert_eq!(graph_ahead_behind(&repo, second, base), Some((2, 0)));
        assert_eq!(
            graph_ahead_behind(&repo, second, base),
            repo.graph_ahead_behind(second, base).ok()
        );
    }
}
