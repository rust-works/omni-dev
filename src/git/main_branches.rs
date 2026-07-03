//! Detection of remote main-branch tips and commit containment.

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::git::remote::RemoteInfo;

/// A resolved remote main branch: display name plus the commit it points at.
#[derive(Debug, Clone)]
pub struct MainBranchTip {
    /// Display name, e.g. `origin/main`.
    pub name: String,
    /// Commit the local remote-tracking ref currently points at.
    pub tip: Oid,
}

/// Resolves the main-branch tip for every remote, using only local refs.
///
/// Works offline: detection never shells out to `gh` or touches the network.
/// Remotes whose main branch cannot be determined locally are skipped, so an
/// unresolvable remote contributes no tip rather than a wrong one.
pub fn detect_main_branch_tips(repo: &Repository) -> Result<Vec<MainBranchTip>> {
    let mut tips = Vec::new();
    let remote_names = repo.remotes().context("Failed to get remote names")?;

    for remote_name in remote_names.iter().flatten().flatten() {
        let Some(branch_name) = RemoteInfo::detect_main_branch_local(repo, remote_name) else {
            continue;
        };

        let reference_name = format!("refs/remotes/{remote_name}/{branch_name}");
        let Ok(reference) = repo.find_reference(&reference_name) else {
            continue;
        };
        let Ok(commit) = reference.peel_to_commit() else {
            continue;
        };

        tips.push(MainBranchTip {
            name: format!("{remote_name}/{branch_name}"),
            tip: commit.id(),
        });
    }

    Ok(tips)
}

/// Returns the display names of the given main branches that contain
/// `commit_oid` (the commit is the branch tip or an ancestor of it).
pub fn branches_containing(
    repo: &Repository,
    tips: &[MainBranchTip],
    commit_oid: Oid,
) -> Result<Vec<String>> {
    let mut containing = Vec::new();

    for tip in tips {
        let contained = commit_oid == tip.tip
            || repo
                .graph_descendant_of(tip.tip, commit_oid)
                .with_context(|| {
                    format!("Failed to check whether {} contains {commit_oid}", tip.name)
                })?;
        if contained {
            containing.push(tip.name.clone());
        }
    }

    Ok(containing)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Runs `git` in `dir` with a deterministic identity, asserting success.
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?} failed: {stderr}");
    }

    /// Builds a work repo on `branch` with one commit pushed to a bare
    /// `origin` remote. Both temp dirs are returned so the caller keeps them
    /// alive for the duration of the test.
    fn repo_with_pushed_branch(branch: &str) -> (tempfile::TempDir, tempfile::TempDir, Repository) {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let bare = tempfile::tempdir_in(&tmp_root).unwrap();
        git_in(bare.path(), &["init", "--bare"]);

        let work = tempfile::tempdir_in(&tmp_root).unwrap();
        git_in(work.path(), &["init"]);
        git_in(work.path(), &["checkout", "-b", branch]);
        std::fs::write(work.path().join("file.txt"), "content").unwrap();
        git_in(work.path(), &["add", "."]);
        git_in(work.path(), &["commit", "-m", "initial"]);
        git_in(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git_in(work.path(), &["push", "origin", branch]);

        let repo = Repository::open(work.path()).unwrap();
        (work, bare, repo)
    }

    fn add_commit(dir: &std::path::Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), message).unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-m", message]);
    }

    #[test]
    fn tips_resolve_via_common_name_after_push() {
        let (_work, _bare, repo) = repo_with_pushed_branch("main");
        let tips = detect_main_branch_tips(&repo).unwrap();
        assert_eq!(tips.len(), 1);
        assert_eq!(tips[0].name, "origin/main");
        let pushed = repo.refname_to_id("refs/remotes/origin/main").unwrap();
        assert_eq!(tips[0].tip, pushed);
    }

    #[test]
    fn tips_resolve_via_symbolic_head_for_uncommon_branch_name() {
        // `trunk` is not in the common-names fallback, so detection must come
        // from the remote's symbolic HEAD alone.
        let (work, _bare, repo) = repo_with_pushed_branch("trunk");
        git_in(work.path(), &["remote", "set-head", "origin", "trunk"]);
        let tips = detect_main_branch_tips(&repo).unwrap();
        assert_eq!(tips.len(), 1);
        assert_eq!(tips[0].name, "origin/trunk");
    }

    #[test]
    fn unresolvable_remote_contributes_no_tip() {
        // No symbolic HEAD and no common branch name: the remote is skipped
        // rather than guessing that `exotic` is a main branch.
        let (_work, _bare, repo) = repo_with_pushed_branch("exotic");
        let tips = detect_main_branch_tips(&repo).unwrap();
        assert!(tips.is_empty(), "expected no tips, got: {tips:?}");
    }

    #[test]
    fn repo_without_remotes_has_no_tips() {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let work = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo = Repository::init(work.path()).unwrap();
        let tips = detect_main_branch_tips(&repo).unwrap();
        assert!(tips.is_empty());
    }

    #[test]
    fn branches_containing_distinguishes_pushed_from_unpushed() {
        let (work, _bare, repo) = repo_with_pushed_branch("main");
        let pushed = repo.refname_to_id("refs/remotes/origin/main").unwrap();
        add_commit(work.path(), "new.txt", "unpushed change");
        let unpushed = repo.head().unwrap().target().unwrap();

        let tips = detect_main_branch_tips(&repo).unwrap();
        // The pushed commit is the tip itself (equality case).
        assert_eq!(
            branches_containing(&repo, &tips, pushed).unwrap(),
            vec!["origin/main".to_string()]
        );
        assert!(branches_containing(&repo, &tips, unpushed)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn branches_containing_includes_ancestors_of_tip() {
        let (work, _bare, repo) = repo_with_pushed_branch("main");
        let first = repo.refname_to_id("refs/remotes/origin/main").unwrap();
        add_commit(work.path(), "second.txt", "second");
        git_in(work.path(), &["push", "origin", "main"]);

        let tips = detect_main_branch_tips(&repo).unwrap();
        assert_eq!(
            branches_containing(&repo, &tips, first).unwrap(),
            vec!["origin/main".to_string()]
        );
    }
}
