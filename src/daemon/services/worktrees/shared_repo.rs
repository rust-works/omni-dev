//! One `Repository` per repo, not per worktree, for the `ahead-behind` op (#2121).
//!
//! `Repository::discover(worktree)` opens a whole repository rooted at that
//! worktree: it reads and parses the config, builds the ref and object databases,
//! and does it again for every linked worktree of the same repo, though everything
//! the divergence walk reads — branches, remote-tracking refs, config, the object
//! database — belongs to the repo's *common dir* and is identical from each. A
//! batch over N worktrees therefore paid for N opens to answer from one repo.
//!
//! This module answers from a handle opened **at the common dir** instead:
//!
//! - [`locate`] finds a folder's common dir and, for a linked worktree, its name,
//!   without opening anything (libgit2's own `.git`-file discovery, so
//!   subdirectories and gitlinks resolve exactly as `discover` resolves them).
//! - [`head_of`] reads that worktree's HEAD through the common dir handle, via the
//!   `worktrees/<name>/HEAD` pseudo-reference libgit2 resolves in the main
//!   repository's ref database — `git2` does not bind
//!   `git_repository_head_for_worktree`, and nothing here needs `unsafe`.
//! - [`RepoPool`] hands those handles out. `Repository` is `Send` but not `Sync`,
//!   so a computation *leases* one for its blocking thread and gives it back.
//!
//! The shared path must give the answer `discover` would, so it answers
//! [`None`] — "ask the old path" — whenever it cannot be sure: a HEAD it cannot
//! read, a common dir it cannot open, or a repository whose config can differ
//! between a worktree and the common dir ([`config_differs_per_worktree`]).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use git2::{ErrorCode, Reference, Repository};

use super::divergence::Divergence;

/// The divergence of the worktree at `folder`, read through a handle from `pool`.
///
/// `None` when the shared path cannot answer exactly, in which case the caller
/// must use [`super::folder_divergence`]. A folder that is not in a repository at
/// all is a definite answer — `Repository::discover` fails on the same input — as
/// is an unborn HEAD, which `repo.head()` reports as an error too.
pub(super) fn divergence(pool: &RepoPool, folder: &Path) -> Option<Divergence> {
    divergence_at(pool, locate(folder))
}

/// [`divergence`] once the folder has been [`locate`]d.
fn divergence_at(pool: &RepoPool, located: Located) -> Option<Divergence> {
    let location = match located {
        Located::NotARepo => return Some(Divergence::default()),
        Located::Unclear => return None,
        Located::Repo(location) => location,
    };
    pool.with_repo(&location.commondir, |repo| {
        divergence_in(repo, location.worktree.as_deref())
    })?
}

/// [`divergence`] once a handle at the common dir is in hand.
fn divergence_in(repo: &Repository, worktree: Option<&str>) -> Option<Divergence> {
    match head_of(repo, worktree) {
        Head::Ref(head) => Some(super::head_divergence(repo, head)),
        Head::Unborn => Some(Divergence::default()),
        Head::Unreadable => None,
    }
}

/// Where a folder's repository lives, found without opening it.
#[derive(Debug, PartialEq, Eq)]
struct Location {
    /// The repository's common dir, canonicalized: what [`RepoPool`] keys handles
    /// by, and what every worktree of one repo agrees on.
    commondir: PathBuf,
    /// The linked worktree's name under `<commondir>/worktrees/`, or `None` for
    /// the main working tree (or any repository with no linked worktrees).
    worktree: Option<String>,
}

/// The outcome of [`locate`].
#[derive(Debug, PartialEq, Eq)]
enum Located {
    /// libgit2 finds no repository from the folder, so `discover` fails too.
    NotARepo,
    /// A repository exists but its layout could not be read; use the old path.
    Unclear,
    Repo(Location),
}

/// Finds the repository containing `folder`, walking up from it exactly as
/// `Repository::discover` does, but stopping at the gitdir: no config is read and
/// no repository is opened.
fn locate(folder: &Path) -> Located {
    match Repository::discover_path(folder, std::iter::empty::<&OsStr>()) {
        Ok(gitdir) => locate_in(&gitdir),
        Err(_) => Located::NotARepo,
    }
}

/// Classifies a discovered `gitdir`. A linked worktree's gitdir is
/// `<commondir>/worktrees/<name>` and holds a `commondir` file naming the shared
/// directory; anything else is its own common dir.
fn locate_in(gitdir: &Path) -> Located {
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(relative) => {
            let name = gitdir.file_name().and_then(OsStr::to_str);
            match (name, std::fs::canonicalize(gitdir.join(relative.trim()))) {
                (Some(name), Ok(commondir)) => Located::Repo(Location {
                    commondir,
                    worktree: Some(name.to_string()),
                }),
                _ => Located::Unclear,
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            std::fs::canonicalize(gitdir).map_or(Located::Unclear, |commondir| {
                Located::Repo(Location {
                    commondir,
                    worktree: None,
                })
            })
        }
        Err(_) => Located::Unclear,
    }
}

/// A worktree's HEAD as read through a common dir handle.
enum Head<'r> {
    /// The reference HEAD resolves to: a branch, or itself when detached.
    Ref(Reference<'r>),
    /// HEAD names a branch that has no commit yet. `repo.head()` errors here too.
    Unborn,
    /// The HEAD could not be read, so nothing can be said about it.
    Unreadable,
}

/// Classifies the outcome of resolving a HEAD. Only the two errors that mean "no
/// commit there" are an answer; any other failure (an I/O or lock error, an
/// unusual layout) is left to the per-worktree open rather than guessed to be an
/// empty result.
fn head_from(resolved: Result<Reference<'_>, git2::Error>) -> Head<'_> {
    match resolved {
        Ok(head) => Head::Ref(head),
        Err(e) if matches!(e.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => Head::Unborn,
        Err(_) => Head::Unreadable,
    }
}

/// Reads the HEAD of `worktree` (the main working tree when `None`) through
/// `repo`, which is opened at the common dir.
///
/// A linked worktree's HEAD is not `repo`'s own, so it is read from the
/// `worktrees/<name>/HEAD` pseudo-reference: symbolic for an attached HEAD,
/// direct for a detached one. Resolving it follows the branch through the shared
/// ref database, exactly as `repo.head()` does for a repository rooted at the
/// worktree.
fn head_of<'r>(repo: &'r Repository, worktree: Option<&str>) -> Head<'r> {
    let Some(name) = worktree else {
        return head_from(repo.head());
    };
    match repo.find_reference(&format!("worktrees/{name}/HEAD")) {
        Ok(pseudo) => head_from(pseudo.resolve()),
        Err(_) => Head::Unreadable,
    }
}

/// Whether `repo`'s config can read differently for one of its worktrees than for
/// the common dir this handle is rooted at. If so the handle cannot stand in for
/// the worktree, and the repo is not served from a shared handle.
///
/// Two things do that. `extensions.worktreeConfig` (which `git sparse-checkout`
/// turns on) lets a worktree's own `config.worktree` override `branch.<name>.*` or
/// `remote.*`. And a conditional include (`includeIf`) is evaluated against the
/// repository it is loaded for — libgit2 matches `gitdir:` against its gitdir and
/// `onbranch:` against that gitdir's HEAD — so a handle at the common dir would
/// match them against the common dir and the main working tree instead. An
/// unreadable config counts as differing.
fn config_differs_per_worktree(repo: &Repository) -> bool {
    let Ok(config) = repo.config() else {
        return true;
    };
    if config
        .get_bool("extensions.worktreeConfig")
        .unwrap_or(false)
    {
        return true;
    }
    let mut conditional = false;
    let walked = config
        .entries(Some(r"^includeif\."))
        .and_then(|entries| entries.for_each(|_| conditional = true));
    walked.is_err() || conditional
}

/// How long a pooled handle may be reused after it was opened.
///
/// A pool lives only while `ahead-behind` work is outstanding, which on its own is
/// no bound: windows that keep asking in overlapping batches would keep one alive
/// indefinitely. So this is the bound, on how old a view of a repository a request
/// can be answered from. It is far longer than a batch takes, so a batch still
/// shares one handle throughout.
const MAX_HANDLE_AGE: Duration = Duration::from_secs(30);

/// A bounded pool of `Repository` handles opened at common dirs.
///
/// The bound is the point: at most `limit` handles are live (leased plus idle) at
/// once, matching [`super::divergence::AheadBehindCoordinator`]'s cap on
/// concurrent computations, so sharing handles never raises the number of open
/// repositories above what #2111 allowed. Reaching it evicts the oldest idle
/// handle rather than waiting.
///
/// The pool is meant to live only while `ahead-behind` work is outstanding (the
/// coordinator holds it weakly), so handles do not outlive the requests they
/// served. Because overlapping requests can keep it alive, a handle is also never
/// reused once it is [`MAX_HANDLE_AGE`] old.
pub(super) struct RepoPool {
    limit: usize,
    max_age: Duration,
    state: Mutex<PoolState>,
    opens: AtomicUsize,
}

/// A handle waiting for its next computation.
struct Idle {
    commondir: PathBuf,
    /// When the handle was opened. Reuse does not renew it: it bounds how stale a
    /// view of the repository the handle can give.
    opened: Instant,
    repo: Repository,
}

#[derive(Default)]
struct PoolState {
    /// Handles waiting for their next computation, oldest first.
    idle: Vec<Idle>,
    /// Handles currently leased, or being opened.
    in_use: usize,
    /// Common dirs this pool failed to open or declined to serve
    /// ([`config_differs_per_worktree`]), remembered so a repo that cannot be
    /// served is not reopened once per worktree. Callers fall back to the
    /// per-worktree open for these, which is exactly what happened before the pool.
    refused: HashSet<PathBuf>,
}

impl PoolState {
    /// Removes the oldest idle handles until the live total fits `limit`, and
    /// returns them so the caller drops them after releasing the lock.
    fn evict_to_fit(&mut self, limit: usize) -> Vec<Repository> {
        let excess = (self.in_use + self.idle.len())
            .saturating_sub(limit)
            .min(self.idle.len());
        self.idle.drain(..excess).map(|idle| idle.repo).collect()
    }

    /// Removes the idle handles that have reached `max_age`, for the caller to drop
    /// after releasing the lock.
    fn drain_aged(&mut self, max_age: Duration) -> Vec<Repository> {
        let (aged, fresh): (Vec<_>, Vec<_>) = std::mem::take(&mut self.idle)
            .into_iter()
            .partition(|idle| idle.opened.elapsed() >= max_age);
        self.idle = fresh;
        aged.into_iter().map(|idle| idle.repo).collect()
    }
}

impl RepoPool {
    pub(super) fn new(limit: usize) -> Self {
        Self::with_max_age(limit, MAX_HANDLE_AGE)
    }

    fn with_max_age(limit: usize, max_age: Duration) -> Self {
        Self {
            limit,
            max_age,
            state: Mutex::new(PoolState::default()),
            opens: AtomicUsize::new(0),
        }
    }

    /// Runs `f` on a handle opened at `commondir` — an idle one if the pool has
    /// it, else a fresh open — then keeps the handle for the next caller.
    ///
    /// `None` when no usable handle can be had: the open failed, or the repo's
    /// config can differ per worktree.
    pub(super) fn with_repo<R>(
        &self,
        commondir: &Path,
        f: impl FnOnce(&Repository) -> R,
    ) -> Option<R> {
        let lease = self.lease(commondir)?;
        let result = f(&lease);
        lease.give_back();
        Some(result)
    }

    fn lease(&self, commondir: &Path) -> Option<Lease<'_>> {
        let (reused, evicted) = {
            let mut state = self.lock();
            if state.refused.contains(commondir) {
                return None;
            }
            state.in_use += 1;
            let mut evicted = state.drain_aged(self.max_age);
            let reused = state
                .idle
                .iter()
                .position(|idle| idle.commondir == commondir)
                .map(|at| state.idle.remove(at));
            // A miss adds a handle, so make room for it first. A hit swaps an
            // idle handle for a leased one and the total is unchanged.
            if reused.is_none() {
                evicted.extend(state.evict_to_fit(self.limit));
            }
            (reused, evicted)
        };
        drop(evicted);
        // From here the slot is released on every exit, including the `?` below
        // and a panic in the caller.
        let slot = Slot {
            pool: self,
            armed: true,
        };
        let (opened, repo) = match reused {
            Some(idle) => (idle.opened, idle.repo),
            None => (Instant::now(), self.open(commondir)?),
        };
        Some(Lease {
            slot,
            commondir: commondir.to_path_buf(),
            opened,
            repo,
        })
    }

    /// Opens a handle at `commondir`, or remembers that it cannot be served.
    fn open(&self, commondir: &Path) -> Option<Repository> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        let opened = Repository::open(commondir)
            .ok()
            .filter(|repo| !config_differs_per_worktree(repo));
        if opened.is_none() {
            self.lock().refused.insert(commondir.to_path_buf());
        }
        opened
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many repositories this pool has tried to open.
    #[cfg(test)]
    pub(super) fn opens(&self) -> usize {
        self.opens.load(Ordering::Relaxed)
    }

    /// Handles currently live: leased plus idle.
    #[cfg(test)]
    fn live(&self) -> usize {
        let state = self.lock();
        state.in_use + state.idle.len()
    }
}

/// One leased unit of capacity. Dropping it frees the capacity, which is what
/// keeps the count honest when a lease is abandoned rather than given back.
struct Slot<'p> {
    pool: &'p RepoPool,
    armed: bool,
}

impl Slot<'_> {
    /// Retires the slot without the drop-time release, once its capacity has
    /// been accounted for elsewhere.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self.pool.lock();
            state.in_use = state.in_use.saturating_sub(1);
        }
    }
}

/// A handle checked out of a [`RepoPool`]. Dropping it discards the handle and
/// frees its capacity; [`Lease::give_back`] keeps the handle for reuse.
struct Lease<'p> {
    slot: Slot<'p>,
    commondir: PathBuf,
    /// When the handle was first opened, carried through reuse.
    opened: Instant,
    repo: Repository,
}

impl Deref for Lease<'_> {
    type Target = Repository;

    fn deref(&self) -> &Repository {
        &self.repo
    }
}

impl Lease<'_> {
    /// Returns the handle to the pool as idle, evicting the oldest idle handle if
    /// that takes the pool over its limit.
    fn give_back(self) {
        let Lease {
            slot,
            commondir,
            opened,
            repo,
        } = self;
        let evicted = {
            let mut state = slot.pool.lock();
            state.in_use = state.in_use.saturating_sub(1);
            state.idle.push(Idle {
                commondir,
                opened,
                repo,
            });
            state.evict_to_fit(slot.pool.limit)
        };
        slot.disarm();
        drop(evicted);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    fn signature() -> git2::Signature<'static> {
        git2::Signature::now("Test", "test@example.com").unwrap()
    }

    /// A repository with one empty commit on `main`, which `HEAD` names.
    fn repo_with_commit(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        {
            let tree = repo
                .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let sig = signature();
            repo.commit(Some("refs/heads/main"), &sig, &sig, "A", &tree, &[])
                .unwrap();
        }
        repo.set_head("refs/heads/main").unwrap();
        repo
    }

    /// Adds a linked worktree named `name` at `path`, on a new branch of the same
    /// name cut from `main`.
    fn add_linked(repo: &Repository, name: &str, path: &Path) {
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch(name, &tip, false).unwrap();
        let reference = repo.find_reference(&format!("refs/heads/{name}")).unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree(name, path, Some(&opts)).unwrap();
    }

    fn commondir_of(repo: &Repository) -> PathBuf {
        std::fs::canonicalize(repo.path()).unwrap()
    }

    #[test]
    fn locate_names_the_main_checkout_and_a_linked_worktree() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(main_dir.path());
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = wt_dir.path().join("feature");
        add_linked(&repo, "feature", &wt);
        let commondir = commondir_of(&repo);

        assert_eq!(
            locate(main_dir.path()),
            Located::Repo(Location {
                commondir: commondir.clone(),
                worktree: None
            })
        );
        let linked = Located::Repo(Location {
            commondir,
            worktree: Some("feature".to_string()),
        });
        assert_eq!(locate(&wt), linked);

        // A subdirectory resolves to the worktree that contains it, as `discover`
        // does.
        std::fs::create_dir(wt.join("sub")).unwrap();
        assert_eq!(locate(&wt.join("sub")), linked);
    }

    #[test]
    fn locate_reports_a_folder_outside_any_repository() {
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(locate(plain.path()), Located::NotARepo);
        assert_eq!(locate(&plain.path().join("missing")), Located::NotARepo);
    }

    #[test]
    fn locate_in_is_unclear_when_the_layout_cannot_be_read() {
        let gitdir = tempfile::tempdir().unwrap();
        // A `commondir` that is not a readable file.
        std::fs::create_dir(gitdir.path().join("commondir")).unwrap();
        assert_eq!(locate_in(gitdir.path()), Located::Unclear);

        // A `commondir` naming a directory that does not exist.
        let dangling = tempfile::tempdir().unwrap();
        std::fs::write(dangling.path().join("commondir"), "../does-not-exist\n").unwrap();
        assert_eq!(locate_in(dangling.path()), Located::Unclear);

        // No `commondir` and no gitdir to canonicalize.
        assert_eq!(locate_in(&gitdir.path().join("missing")), Located::Unclear);
    }

    #[test]
    fn head_of_reads_every_head_shape_through_the_common_dir() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(main_dir.path());
        let wts = tempfile::tempdir().unwrap();
        for name in ["attached", "detached", "unborn"] {
            add_linked(&repo, name, &wts.path().join(name));
        }
        let tip = repo.head().unwrap().target().unwrap();
        Repository::open(wts.path().join("detached"))
            .unwrap()
            .set_head_detached(tip)
            .unwrap();
        Repository::open(wts.path().join("unborn"))
            .unwrap()
            .set_head("refs/heads/not-yet")
            .unwrap();

        // The main working tree reads the repository's own HEAD.
        let Head::Ref(main) = head_of(&repo, None) else {
            panic!("the main HEAD should resolve");
        };
        assert_eq!(main.name(), Ok("refs/heads/main"));
        // An attached worktree resolves to its own branch, not the main one.
        let Head::Ref(attached) = head_of(&repo, Some("attached")) else {
            panic!("an attached HEAD should resolve");
        };
        assert_eq!(attached.name(), Ok("refs/heads/attached"));
        // A detached worktree resolves to a commit, not a branch.
        let Head::Ref(detached) = head_of(&repo, Some("detached")) else {
            panic!("a detached HEAD should resolve");
        };
        assert!(!detached.is_branch());
        assert_eq!(detached.target(), Some(tip));
        // A branch with no commit is unborn; a worktree git does not know about
        // is unreadable.
        assert!(matches!(head_of(&repo, Some("unborn")), Head::Unborn));
        assert!(matches!(head_of(&repo, Some("ghost")), Head::Unreadable));

        // And a repository with no commits has an unborn main HEAD.
        let empty_dir = tempfile::tempdir().unwrap();
        let empty = Repository::init(empty_dir.path()).unwrap();
        assert!(matches!(head_of(&empty, None), Head::Unborn));
    }

    #[test]
    fn a_pool_is_usable_from_several_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RepoPool>();
    }

    #[test]
    fn a_handle_is_reused_for_the_same_common_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(dir.path());
        let pool = RepoPool::new(4);

        for _ in 0..5 {
            assert_eq!(pool.with_repo(&commondir_of(&repo), |_| ()), Some(()));
        }

        assert_eq!(pool.opens(), 1);
        assert_eq!(pool.live(), 1);
    }

    #[test]
    fn live_handles_never_exceed_the_limit() {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let commondirs: Vec<_> = dirs
            .iter()
            .map(|dir| commondir_of(&repo_with_commit(dir.path())))
            .collect();
        let pool = RepoPool::new(2);

        // Two idle handles fill the pool.
        pool.with_repo(&commondirs[0], |_| ()).unwrap();
        pool.with_repo(&commondirs[1], |_| ()).unwrap();
        assert_eq!(pool.live(), 2);

        // A third repo evicts the oldest idle handle to make room for itself, so
        // the total is still two while it is in use and after it is parked.
        pool.with_repo(&commondirs[2], |_| assert_eq!(pool.live(), 2))
            .unwrap();
        assert_eq!(pool.live(), 2);
        assert_eq!(pool.opens(), 3);

        // The evicted repo must be reopened; the survivors are reused.
        pool.with_repo(&commondirs[2], |_| ()).unwrap();
        assert_eq!(pool.opens(), 3);
        pool.with_repo(&commondirs[0], |_| ()).unwrap();
        assert_eq!(pool.opens(), 4);
        assert_eq!(pool.live(), 2);
    }

    #[test]
    fn a_failed_open_frees_its_capacity_and_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let pool = RepoPool::new(1);

        assert!(pool
            .with_repo(&dir.path().join("missing"), |_| ())
            .is_none());
        assert!(pool
            .with_repo(&dir.path().join("missing"), |_| ())
            .is_none());

        assert_eq!(pool.live(), 0);
        // Remembered after the first failure, so the second call never reopens.
        assert_eq!(pool.opens(), 1);
    }

    #[test]
    fn a_panicking_computation_frees_its_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(dir.path());
        let pool = RepoPool::new(1);

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            pool.with_repo(&commondir_of(&repo), |_| panic!("computation failed"))
        }));

        assert!(outcome.is_err());
        // The handle is discarded rather than parked, and the slot is released.
        assert_eq!(pool.live(), 0);
        assert!(pool.with_repo(&commondir_of(&repo), |_| ()).is_some());
    }

    #[test]
    fn a_repo_whose_config_can_differ_per_worktree_is_refused_once() {
        // Each of these can make a worktree read config the common dir does not.
        for (key, value) in [
            ("extensions.worktreeConfig", "true"),
            ("includeIf.onbranch:feature/**.path", "extra.cfg"),
            ("includeIf.gitdir:/somewhere/.path", "extra.cfg"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = repo_with_commit(dir.path());
            repo.config().unwrap().set_str(key, value).unwrap();
            let pool = RepoPool::new(4);

            assert!(
                pool.with_repo(&commondir_of(&repo), |_| ()).is_none(),
                "{key}"
            );
            assert!(
                pool.with_repo(&commondir_of(&repo), |_| ()).is_none(),
                "{key}"
            );

            // Declined on the first open and remembered, not reopened per worktree.
            assert_eq!(pool.opens(), 1, "{key}");
            assert_eq!(pool.live(), 0, "{key}");
        }
    }

    #[test]
    fn an_unconditional_include_does_not_stop_a_repo_being_shared() {
        // Evaluated the same way whichever repository loads it.
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(dir.path());
        repo.config()
            .unwrap()
            .set_str("include.path", "extra.cfg")
            .unwrap();
        let pool = RepoPool::new(4);

        assert_eq!(pool.with_repo(&commondir_of(&repo), |_| ()), Some(()));
    }

    #[test]
    fn a_handle_is_not_reused_once_it_reaches_its_age_limit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(dir.path());
        // A zero limit means every handle is already too old to reuse.
        let pool = RepoPool::with_max_age(4, Duration::ZERO);

        for _ in 0..3 {
            assert_eq!(pool.with_repo(&commondir_of(&repo), |_| ()), Some(()));
        }

        assert_eq!(pool.opens(), 3);
        // The aged handle is dropped when the next lease finds it, so the pool
        // never holds more than the one just returned.
        assert_eq!(pool.live(), 1);
    }

    #[test]
    fn reuse_does_not_renew_a_handles_age() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(dir.path());
        let pool = RepoPool::new(4);
        let commondir = commondir_of(&repo);
        pool.with_repo(&commondir, |_| ()).unwrap();
        let opened = pool.lock().idle[0].opened;

        pool.with_repo(&commondir, |_| ()).unwrap();

        assert_eq!(pool.opens(), 1);
        assert_eq!(pool.lock().idle[0].opened, opened);
    }

    #[test]
    fn only_a_missing_commit_is_an_empty_head() {
        let error = |code| git2::Error::new(code, git2::ErrorClass::Reference, "test");
        assert!(matches!(
            head_from(Err(error(ErrorCode::UnbornBranch))),
            Head::Unborn
        ));
        assert!(matches!(
            head_from(Err(error(ErrorCode::NotFound))),
            Head::Unborn
        ));
        // Anything else is unknown, and left to the per-worktree open.
        for code in [
            ErrorCode::GenericError,
            ErrorCode::Locked,
            ErrorCode::Invalid,
        ] {
            assert!(matches!(head_from(Err(error(code))), Head::Unreadable));
        }
    }

    #[test]
    fn a_handle_at_the_common_dir_sees_the_shared_shallow_marker() {
        // The walk memo is bypassed when the handle reports `is_shallow()`, which
        // reads `<commondir>/shallow`. A linked worktree shares that file, so the
        // handle the pool serves for its repo must see it too.
        let main_dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(main_dir.path());
        let wt_dir = tempfile::tempdir().unwrap();
        add_linked(&repo, "feature", &wt_dir.path().join("feature"));
        let tip = repo.head().unwrap().target().unwrap();
        let pool = RepoPool::new(4);
        let commondir = commondir_of(&repo);

        assert_eq!(
            pool.with_repo(&commondir, Repository::is_shallow),
            Some(false)
        );
        std::fs::write(repo.path().join("shallow"), format!("{tip}\n")).unwrap();
        assert_eq!(
            pool.with_repo(&commondir, Repository::is_shallow),
            Some(true)
        );
    }

    #[test]
    fn whatever_cannot_be_read_exactly_defers_to_the_discover_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo_with_commit(dir.path());
        let pool = RepoPool::new(4);
        let at = |worktree: Option<&str>, commondir: PathBuf| {
            divergence_at(
                &pool,
                Located::Repo(Location {
                    commondir,
                    worktree: worktree.map(str::to_string),
                }),
            )
        };

        // A layout that could not be read, a common dir that will not open, and a
        // worktree whose HEAD cannot be read each answer "ask the old path".
        assert_eq!(divergence_at(&pool, Located::Unclear), None);
        assert_eq!(at(None, dir.path().join("missing")), None);
        assert_eq!(at(Some("ghost"), commondir_of(&repo)), None);

        // A folder in no repository, by contrast, is a definite empty answer, and
        // so is the unborn main HEAD of a repository with no commits.
        assert_eq!(
            divergence_at(&pool, Located::NotARepo),
            Some(Divergence::default())
        );
        let empty_dir = tempfile::tempdir().unwrap();
        let empty = Repository::init(empty_dir.path()).unwrap();
        assert_eq!(at(None, commondir_of(&empty)), Some(Divergence::default()));
    }
}
