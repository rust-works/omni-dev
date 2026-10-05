//! Reading history through a shallow clone's cut (#2147, #2163).
//!
//! git keeps a shallow clone's `shallow` marker in the **common dir**, which every
//! worktree of the repository shares. libgit2 looks for it, and applies the cut it
//! lists, only through the gitdir of the handle it opened. For a handle rooted at a
//! linked worktree that gitdir is `<commondir>/worktrees/<name>`, so the handle
//! reports a shallow clone as complete and does not apply the cut: a walk through it
//! runs into the parents a `--depth` clone does not have and fails with "object not
//! found". The two agree for a main checkout, whose gitdir is the common dir.
//!
//! Anything that compares history in a repository a user may have cloned with
//! `--depth` therefore reads it through this module rather than through the handle
//! it was given: the daemon's `ahead-behind` op (`divergence`) and the batch push
//! classifier ([`worktree_push`]), which also asks whether the cut leaves a divergence
//! provable ([`divergence_is_provable`], #2175). Reading is all it does — nothing here
//! writes to the repository.
//!
//! [`worktree_push`]: crate::git::worktree_push

use git2::{Oid, Repository};

/// Whether `repo` is shallow, as git sees it: its common dir holds a non-empty
/// `shallow` file.
///
/// `Repository::is_shallow` is not that. libgit2 looks for the marker in the
/// handle's own gitdir, which for a repository opened at a linked worktree is
/// `<commondir>/worktrees/<name>`, while git keeps `shallow` in the common dir that
/// every worktree shares. So that handle reports a shallow clone as complete
/// (#2147). The two agree for a main checkout, whose gitdir is the common dir.
/// The marker rule is libgit2's — a non-empty file — applied to the common dir.
pub(crate) fn is_shallow(repo: &Repository) -> bool {
    repo.is_shallow()
        || std::fs::metadata(repo.commondir().join("shallow"))
            .is_ok_and(|marker| marker.is_file() && marker.len() > 0)
}

/// Commits `local` is ahead of and behind `upstream`, with a shallow repository's
/// cut applied. Not memoized.
///
/// A linked worktree of a shallow repository is walked through a handle opened at
/// the common dir, the only kind through which libgit2 applies the cut; every other
/// shape (a complete repository, a main checkout) is walked through `repo` itself,
/// which is exact for it. A failed open of the common dir is `None` — no counts —
/// rather than counts from the uncut history, and is logged at `debug` (not `warn`:
/// a caller that retries on every ask would repeat a persistent failure).
///
/// `None` also when the walk itself fails, which means a tip has no commit behind
/// it in this object database (or, for a handle that was not opened at the right
/// place, the parents of the cut). A caller must treat that as *unknown*, never as
/// "no divergence".
pub(crate) fn graph_ahead_behind(
    repo: &Repository,
    local: Oid,
    upstream: Oid,
) -> Option<(usize, usize)> {
    with_cut_applied(repo, |walker| {
        walker.graph_ahead_behind(local, upstream).ok()
    })?
}

/// Whether "ahead *and* behind" between `local` and `upstream` proves that they
/// diverged (#2175).
///
/// In a complete repository it does, so this is `true`. In a shallow one it proves
/// divergence only when the cut leaves a common ancestor visible: `git fetch --depth`
/// can make the upstream a shallow root whose parent — the local tip — is hidden, and
/// then a branch that is merely *behind* counts as 1 ahead and 1 behind, identical to
/// one that truly diverged. Without a visible merge base the two cannot be told apart,
/// so a caller about to act on "diverged" (a force-push) must not.
///
/// A visible merge base is a heuristic, not a proof: a merge commit at the boundary
/// can still hide ancestry through the cut. It removes the no-merge-base case, which
/// is the one a plain `--depth` fetch produces. A walk that fails outright (a tip with
/// no commit behind it, a common dir that will not open) is `false`: unknown is never
/// provable.
pub(crate) fn divergence_is_provable(repo: &Repository, local: Oid, upstream: Oid) -> bool {
    if !is_shallow(repo) {
        return true;
    }
    with_cut_applied(repo, |walker| walker.merge_base(local, upstream).ok())
        .flatten()
        .is_some()
}

/// Runs `walk` against the handle through which `repo`'s history is read with a
/// shallow cut applied: a handle opened at the common dir for a linked worktree of a
/// shallow repository, `repo` itself for every other shape. `None` when that handle
/// cannot be opened, logged at `debug` (see [`graph_ahead_behind`]).
fn with_cut_applied<T>(repo: &Repository, walk: impl FnOnce(&Repository) -> T) -> Option<T> {
    if !(repo.is_worktree() && is_shallow(repo)) {
        return Some(walk(repo));
    }
    match Repository::open(repo.commondir()) {
        Ok(common) => Some(walk(&common)),
        Err(e) => {
            let path = repo.commondir().display();
            tracing::debug!("cannot open {path} to walk a shallow repository: {e}");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::shallow_repo::{
        add_linked_worktree, forget_object, mark_shallow, three_commits,
    };

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

    /// The premise of [`is_shallow`] and [`graph_ahead_behind`], pinned the way
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

    /// The walk must see the cut, which only a handle opened at the common dir does:
    /// with `first` shallow it is a root, so `base` is behind it rather than
    /// reachable from it.
    #[test]
    fn a_linked_worktree_of_a_shallow_repository_is_walked_with_the_cut_applied() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, _second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();
        mark_shallow(&repo, first);

        let common = Repository::open(repo.commondir()).unwrap();
        assert!(common.is_shallow());
        assert_eq!(common.graph_ahead_behind(first, base).unwrap(), (1, 1));
        assert_eq!(graph_ahead_behind(&linked, first, base), Some((1, 1)));
    }

    /// What a real `--depth` clone does to a linked worktree: the commits behind the
    /// cut are not in the object database, so a walk that does not apply the cut
    /// fails on the missing parent and the caller would lose its counts.
    #[test]
    fn a_linked_worktree_of_a_shallow_clone_still_gets_counts_without_the_cut_commits() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let path = add_linked_worktree(&repo, base, wts.path(), "feature");
        mark_shallow(&repo, first);
        forget_object(&repo, base);
        let linked = Repository::open(path).unwrap();

        assert_eq!(graph_ahead_behind(&linked, second, first), Some((1, 0)));
    }

    /// A main checkout is its own common dir, so its handle already applies the cut
    /// and is walked in place. The handle is opened after the marker is written,
    /// because libgit2 reads the shallow grafts when it opens a repository.
    #[test]
    fn a_main_checkout_of_a_shallow_clone_is_walked_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        mark_shallow(&repo, first);
        forget_object(&repo, base);
        let checkout = Repository::open(dir.path()).unwrap();

        assert!(!checkout.is_worktree());
        assert_eq!(graph_ahead_behind(&checkout, second, first), Some((1, 0)));
    }

    /// A complete repository is not touched by any of this: the counts are libgit2's
    /// own, whatever shape of handle asks.
    #[test]
    fn a_complete_repository_gets_libgit2s_counts_from_any_handle() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();

        for handle in [&repo, &linked] {
            assert_eq!(graph_ahead_behind(handle, second, base), Some((2, 0)));
            assert_eq!(
                graph_ahead_behind(handle, first, second),
                handle.graph_ahead_behind(first, second).ok()
            );
        }
    }

    /// A tip with no commit behind it is unknown, which a caller must be able to tell
    /// from "no divergence": the answer is `None`, not `(0, 0)`.
    #[test]
    fn a_tip_with_no_commit_behind_it_has_no_counts() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, _base, first, _second) = three_commits(dir.path());
        let nothing = Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();

        assert_eq!(graph_ahead_behind(&repo, nothing, first), None);
        assert_eq!(graph_ahead_behind(&repo, first, nothing), None);
    }

    /// A common dir that will not open leaves the caller without counts, not with
    /// counts from a handle that ignores the cut, and says why at `debug`. The
    /// message is matched whole, since libgit2's own error text can name the path
    /// too and would satisfy a bare `contains(path)`.
    #[test]
    fn a_common_dir_that_will_not_open_leaves_a_shallow_worktree_without_counts() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();
        mark_shallow(&repo, first);
        // Without `HEAD` the directory is no longer a repository to libgit2.
        std::fs::remove_file(repo.commondir().join("HEAD")).unwrap();
        assert!(Repository::open(repo.commondir()).is_err());

        let mut counts = Some((0, 0));
        let logs = crate::test_support::capture_at(tracing::Level::DEBUG, || {
            counts = graph_ahead_behind(&linked, second, first);
        });

        assert_eq!(counts, None);
        let logged = format!(
            "cannot open {} to walk a shallow repository",
            repo.commondir().display()
        );
        assert!(
            logs.contains(&logged),
            "the failed open was not logged with its path: {logs}"
        );
    }

    /// A commit with no parent, written into `repo`: with `first` made a shallow
    /// root it is what a cut upstream looks like next to a local tip — two roots
    /// sharing no visible ancestor.
    fn unrelated_root(repo: &Repository) -> Oid {
        let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(None, &sig, &sig, "root", &tree, &[]).unwrap()
    }

    /// "Ahead and behind" proves divergence in a complete repository, whatever shape
    /// of handle asks, so nothing is withheld there.
    #[test]
    fn divergence_in_a_complete_repository_is_always_provable() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, _second) = three_commits(dir.path());
        let other = unrelated_root(&repo);
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();

        for handle in [&repo, &linked] {
            assert!(divergence_is_provable(handle, first, other));
        }
    }

    /// The case of #2175: with both tips shallow roots no merge base is visible, so
    /// the two cannot be told from a branch that is only behind.
    #[test]
    fn divergence_with_no_visible_merge_base_in_a_shallow_repository_is_not_provable() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, base, first, _second) = three_commits(dir.path());
        let other = unrelated_root(&repo);
        mark_shallow(&repo, first);
        forget_object(&repo, base);
        let checkout = Repository::open(dir.path()).unwrap();

        assert!(!divergence_is_provable(&checkout, first, other));
    }

    /// A merge base inside the cut is visible, so the verdict stands. Checked through
    /// a linked worktree's handle too, which must be walked at the common dir.
    #[test]
    fn divergence_with_a_visible_merge_base_in_a_shallow_repository_is_provable() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let sibling = repo
            .commit(
                None,
                &sig,
                &sig,
                "sibling",
                &tree,
                &[&repo.find_commit(first).unwrap()],
            )
            .unwrap();
        let path = add_linked_worktree(&repo, base, wts.path(), "feature");
        mark_shallow(&repo, first);
        forget_object(&repo, base);
        let linked = Repository::open(path).unwrap();
        let checkout = Repository::open(dir.path()).unwrap();

        // `second` and `sibling` both descend from the shallow root `first`.
        for handle in [&checkout, &linked] {
            assert!(divergence_is_provable(handle, second, sibling));
        }
    }

    /// Unknown is never provable: a tip with no commit behind it, or a common dir
    /// that will not open, must not read as a divergence to act on.
    #[test]
    fn a_walk_that_cannot_be_answered_is_not_provable() {
        let dir = tempfile::tempdir().unwrap();
        let wts = tempfile::tempdir().unwrap();
        let (repo, base, first, second) = three_commits(dir.path());
        let linked =
            Repository::open(add_linked_worktree(&repo, base, wts.path(), "feature")).unwrap();
        mark_shallow(&repo, first);
        let nothing = Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(!divergence_is_provable(&repo, second, nothing));

        std::fs::remove_file(repo.commondir().join("HEAD")).unwrap();
        assert!(!divergence_is_provable(&linked, second, first));
    }
}
